use std::{fmt, mem};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic::Ordering::Relaxed;
use crate::state::ReleaseState::Unlocked;
use crate::state::AcquireState::{Available, Queued};
use std::fmt::{Debug, Formatter};
use crate::state::{AcquireStep, Waiter, Permits, AcquireState, ReleaseState};
use std::cell::UnsafeCell;
use crate::{AcquireFuture, TryAcquireError, SemaphoreGuard, AcquireFutureArc, SemaphoreGuardArc};
use std::marker::{PhantomPinned, PhantomData};
use crate::waker::AtomicWaker;
use std::ptr::null;
use std::sync::Arc;
use crate::atomic::Atomic;
use std::mem::size_of;
use crate::release::ReleaseAction;
use crate::errors::PoisonError;

/// An async weighted semaphore. See [crate documentation](index.html) for usage.
// This implementation encodes state (the available counter, acquire queue, and cancel queue) into
// multiple atomic variables and linked lists. Concurrent acquires (and concurrent cancels) synchronize
// by pushing onto a stack with an atomic swap. Releases synchronize with other operations by attempting
// to acquire a lock. If the lock is successfully acquired, the release can proceed. Otherwise
// the lock is marked dirty to indicate that there is additional work for the lock owner to do.
pub struct Semaphore {
    // The number of available permits or the back of the queue (without next edges).
    pub(crate) acquire: Atomic<AcquireState>,
    // A number of releasable permits, and the state of the current release lock.
    pub(crate) release: Atomic<ReleaseState>,
    // The front of the queue (with next edges).
    pub(crate) front: UnsafeCell<*const Waiter>,
    // The last node swapped from AcquireState (with next edges).
    pub(crate) middle: UnsafeCell<*const Waiter>,
    // A stack of nodes that are cancelling.
    pub(crate) next_cancel: Atomic<*const Waiter>,
}

unsafe impl Sync for Semaphore {}

unsafe impl Send for Semaphore {}

impl UnwindSafe for Semaphore {}

impl RefUnwindSafe for Semaphore {}

impl Debug for Semaphore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.acquire.load(Relaxed) {
            Available(available) => write!(f, "Semaphore::Ready({:?})", available)?,
            Queued(_) => match self.release.load(Relaxed) {
                Unlocked(available) => write!(f, "Semaphore::Blocked({:?})", available)?,
                _ => write!(f, "Semaphore::Unknown")?,
            },
        };
        Ok(())
    }
}


impl Semaphore {
    /// The maximum number of permits that can be made available. This is slightly smaller than
    /// [`usize::MAX`]. If the number of available permits exceeds this number, it may poison the
    /// semaphore.
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::{Semaphore, SemaphoreGuard};
    /// struct ReadWriteLock(Semaphore);
    /// impl ReadWriteLock {
    ///     fn new() -> Self {
    ///         ReadWriteLock(Semaphore::new(Semaphore::MAX_AVAILABLE))
    ///     }
    ///     // Acquire one permit, allowing up to MAX_AVAILABLE concurrent readers.
    ///     async fn read(&self) -> SemaphoreGuard<'_> {
    ///         self.0.acquire(1).await.unwrap()
    ///     }
    ///     // The writer acquires all the permits, prevent any concurrent writers or readers. The
    ///     // first-in-first-out priority policy prevents writer starvation.
    ///     async fn write(&self) -> SemaphoreGuard<'_> {
    ///         self.0.acquire(Semaphore::MAX_AVAILABLE).await.unwrap()
    ///     }
    /// }
    /// ```
    pub const MAX_AVAILABLE: usize = (1 << (size_of::<usize>() * 8 - 3)) - 1;

    /// Create a new semaphore with an initial number of permits.
    /// # Examples
    /// ```
    /// use async_weighted_semaphore::Semaphore;
    /// let semaphore = Semaphore::new(1024);
    /// ```
    pub fn new(initial: usize) -> Self {
        Semaphore {
            acquire: Atomic::new(Available(Permits::new(initial))),
            release: Atomic::new(Unlocked(Permits::new(0))),
            front: UnsafeCell::new(null()),
            middle: UnsafeCell::new(null()),
            next_cancel: Atomic::new(null()),
        }
    }

    /// Wait until there are no older pending calls to [acquire](#method.acquire) and at least `amount` permits available.
    /// Then consume the requested permits and return a [`SemaphoreGuard`].
    /// # Errors
    /// Returns [`PoisonError`] is the semaphore is poisoned.
    /// # Examples
    /// ```
    /// # use futures::executor::block_on;
    /// # use std::future::Future;
    /// use async_weighted_semaphore::Semaphore;
    /// async fn limit_concurrency(semaphore: &Semaphore, future: impl Future<Output=()>) {
    ///     let guard = semaphore.acquire(1).await.unwrap();
    ///     future.await
    /// }
    /// ```
    pub fn acquire(&self, amount: usize) -> AcquireFuture {
        AcquireFuture(UnsafeCell::new(Waiter {
            semaphore: self,
            step: UnsafeCell::new(AcquireStep::Entering),
            waker: unsafe { AtomicWaker::new() },
            amount,
            next: UnsafeCell::new(null()),
            prev: UnsafeCell::new(null()),
            next_cancel: UnsafeCell::new(null()),
        }), PhantomData, PhantomPinned)
    }

    /// Like [acquire](#method.acquire), but fails if the call would block.
    /// # Errors
    /// * Returns [`TryAcquireError::Poisoned`] is the semaphore is poisoned.
    /// * Returns [`TryAcquireError::WouldBlock`] if a call to `acquire` would have blocked. This can
    /// occur if there are insufficient available permits or if there is another pending call to acquire.
    /// # Examples
    /// ```
    /// # use futures::executor::block_on;
    /// # use std::future::Future;
    /// use async_weighted_semaphore::Semaphore;
    /// async fn run_if_safe(semaphore: &Semaphore, future: impl Future<Output=()>) {
    ///     if semaphore.try_acquire(1).is_ok() {
    ///         future.await
    ///     }
    /// }
    /// ```
    pub fn try_acquire(&self, amount: usize) -> Result<SemaphoreGuard, TryAcquireError> {
        self.acquire.transact(|mut acquire| {
            Ok(match *acquire {
                Queued(_) => Err(TryAcquireError::WouldBlock),
                Available(available) => {
                    match available.into_usize() {
                        Some(available) => {
                            if amount <= available {
                                *acquire = Available(Permits::new(available - amount));
                                acquire.commit()?;
                                Ok(SemaphoreGuard::new(self, amount))
                            } else {
                                Err(TryAcquireError::WouldBlock)
                            }
                        }
                        None => Err(TryAcquireError::Poisoned),
                    }
                }
            })
        })
    }

    /// Like [acquire](#method.acquire), but takes an [`Arc`] `<Semaphore>` and returns a guard that is `'static`, [`Send`] and [`Sync`].
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::{Semaphore, PoisonError, SemaphoreGuardArc};
    /// # use std::sync::Arc;
    /// use async_channel::{Sender, SendError};
    /// // Limit size of a producer-consumer queue
    /// async fn send<T>(semaphore: &Arc<Semaphore>,
    ///                  sender: &Sender<(SemaphoreGuardArc, T)>,
    ///                  message: T
    ///         ) -> Result<(), SendError<T>>{
    ///     match semaphore.acquire_arc(1).await {
    ///         // A semaphore can be poisoned to prevent deadlock when a channel closes.
    ///         Err(PoisonError) => Err(SendError(message)),
    ///         Ok(guard) => match sender.send((guard, message)).await{
    ///             Err(SendError((guard, message))) => Err(SendError(message)),
    ///             Ok(()) => Ok(())
    ///         }
    ///     }
    /// }
    /// ```
    pub fn acquire_arc(self: &Arc<Self>, amount: usize) -> AcquireFutureArc {
        AcquireFutureArc {
            arc: self.clone(),
            inner: unsafe { mem::transmute::<AcquireFuture, AcquireFuture>(self.acquire(amount)) },
        }
    }


    /// Like [try_acquire](#method.try_acquire), but takes an [`Arc`] `<Semaphore>`, and returns a guard that is `'static`,
    /// [`Send`] and [`Sync`].
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::{Semaphore, TryAcquireError, SemaphoreGuardArc};
    /// # use std::sync::Arc;
    /// use async_channel::{Sender, TrySendError};
    /// // Limit size of a producer-consumer queue
    /// async fn try_send<T>(semaphore: &Arc<Semaphore>,
    ///                  sender: &Sender<(SemaphoreGuardArc, T)>,
    ///                  message: T
    ///         ) -> Result<(), TrySendError<T>>{
    ///     match semaphore.try_acquire_arc(1) {
    ///         Err(TryAcquireError::WouldBlock) => Err(TrySendError::Full(message)),
    ///         // A semaphore can be poisoned to prevent deadlock when a channel closes.
    ///         Err(TryAcquireError::Poisoned) => Err(TrySendError::Closed(message)),
    ///         Ok(guard) => match sender.try_send((guard, message)) {
    ///             Err(TrySendError::Closed((guard, message))) => Err(TrySendError::Closed(message)),
    ///             Err(TrySendError::Full((guard, message))) => Err(TrySendError::Full(message)),
    ///             Ok(()) => Ok(())
    ///         }
    ///     }
    /// }
    /// ```
    pub fn try_acquire_arc(self: &Arc<Self>, amount: usize) -> Result<SemaphoreGuardArc, TryAcquireError> {
        let guard = self.try_acquire(amount)?;
        let result = SemaphoreGuardArc::new(self.clone(), amount);
        guard.forget();
        Ok(result)
    }

    /// Return `amount` permits to the semaphore. This will eventually wake any calls to [acquire](#method.acquire)
    /// that can succeed with the additional permits. Calling `release` often makes sense after calling
    /// [`SemaphoreGuard::forget`] or when using the semaphore to signal the number of elements that
    /// are available for processing.
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::{Semaphore, TryAcquireError};
    /// use async_channel::{Receiver, RecvError};
    /// // Limit size of a producer-consumer queue
    /// async fn recv<T>(semaphore: &Semaphore, recv: &Receiver<T>) -> Result<T, RecvError>{
    ///     let result = recv.recv().await?;
    ///     // Note that this only guards elements in the queue, not those being processed after the
    ///     // queue.
    ///     semaphore.release(1);
    ///     Ok(result)
    /// }
    /// ```
    pub fn release(&self, amount: usize) {
        unsafe {
            ReleaseAction { sem: self, releasable: Permits::new(amount) }.release();
        }
    }

    /// Poison the semaphore, causing all pending and future calls to `acquire` to fail immediately.
    /// This can be used to unblock pending acquires when the guarded operation would fail anyway.
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::{Semaphore, TryAcquireError};
    /// # use std::sync::Arc;
    /// # use async_std::sync::Mutex;
    /// use async_channel::{Receiver, RecvError};
    /// async fn consume(semaphore: &Semaphore, receiver: Receiver<usize>){
    ///     while let Ok(x) = receiver.recv().await {
    ///         println!("{:?}", x);
    ///         semaphore.release(1);
    ///     }
    ///     // There will be no more calls to recv, so unblock all senders.
    ///     semaphore.poison();
    /// }
    /// ```
    pub fn poison(&self) {
        unsafe {
            ReleaseAction { sem: self, releasable: Permits::poison() }.release();
        }
    }
}
