//! An asynchronous weighted [`Semaphore`].

#[cfg(test)]
#[macro_use]
extern crate lazy_static;

use std::{fmt, mem, thread};
use std::cell::UnsafeCell;
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::fmt::Display;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::size_of;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::ptr::null;
use std::sync::Arc;
use std::sync::atomic::Ordering::{AcqRel, Relaxed, SeqCst};
use std::task::{Context, Poll};

use crate::atomic::Atomic;
pub use crate::guard::SemaphoreGuard;
pub use crate::guard::SemaphoreGuardArc;
use crate::state::{AcquireState, Permits, ReleaseState, Waiter};
use crate::state::AcquireState::{Available, Queued};
use crate::state::AcquireStep;
use crate::state::ReleaseState::{Locked, LockedDirty, Unlocked};
use crate::waker::{AtomicWaker, WakerResult};

mod atomic;
mod waker;
mod guard;
mod state;
#[cfg(test)]
mod tests;

/// An error returned by [`Semaphore::acquire`] to indicate the Semaphore has been poisoned.
#[derive(Debug, Eq, Ord, PartialOrd, PartialEq, Clone, Copy)]
pub struct AcquireError;

impl Error for AcquireError {}

impl Display for AcquireError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// An error returned from the [`Semaphore::try_acquire`].
#[derive(Debug, Eq, Ord, PartialOrd, PartialEq)]
pub enum TryAcquireError {
    /// [`Semaphore::try_acquire`] failed because [`Semaphore::acquire`] would have blocked. Either
    /// there are insufficient available permits or there is another pending call to acquire.
    WouldBlock,
    /// [`Semaphore::try_acquire`] failed because the `Semaphore` is poisoned.
    Poisoned,
}

impl Error for TryAcquireError {}

impl Display for TryAcquireError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// An async weighted semaphore.
/// A semaphore is a synchronization primitive for limiting concurrent usage of a resource or
/// signaling availability of a resource.
///
/// A `Semaphore` starts with an initial counter of permits. Calling [release](#method.release) will increase the
/// counter. Calling [acquire](#method.acquire) will attempt to decrease the counter, waiting if necessary.
///
/// # Priority
/// Acquiring has "first-in-first-out" semantics: calls to `acquire` finish in the same order that
/// they start. If there is a pending call to `acquire`, a new call to `acquire` will always block,
/// even if there are enough permits available for the new call. This policy reduces starvation and
/// tail latency at the cost of utilization.
/// ```
/// # use async_weighted_semaphore::Semaphore;
/// # use futures::executor::block_on;
/// # block_on(async{
/// # use futures::pin_mut;
/// # use futures::poll;
/// let sem = Semaphore::new(1);
/// let a = sem.acquire(2);
/// let b = sem.acquire(1);
/// pin_mut!(a);
/// pin_mut!(b);
/// assert!(poll!(&mut a).is_pending());
/// assert!(poll!(&mut b).is_pending());
/// # });
/// ```
///
/// # Poisoning
/// If a guard is dropped while panicking, or the number of available permits exceeds [`Self::MAX_AVAILABLE`],
/// the semaphore will be permanently poisoned. All current and future acquires will fail,
/// and release will become a no-op. This is similar in principle to poisoning a [`std::sync::Mutex`].
/// Explicitly poisoning with [`Semaphore::poison`] can also be useful to coordinate termination
/// (e.g. closing a producer-consumer channel).
///
/// # Performance
/// `Semaphore` uses no heap allocations. Most calls are lock-free. The only action that may wait for a
/// lock is cancelling an `AcquireFuture`. In other words, if an `AcquireFuture` is dropped before
/// `AcquireFuture::poll` returns `Poll::Ready`, the drop may synchronously wait for a lock.
// This implementation encodes state (the available counter, acquire queue, and cancel queue) into
// multiple atomic variables and linked lists. Concurrent acquires (and concurrent cancels) synchronize
// by pushing onto a stack with an atomic swap. Releases synchronize with other operations by attempting
// to acquire a lock. If the lock is successfully acquired, the release can proceed. Otherwise
// the lock is marked dirty to indicate that there is additional work for the lock owner to do.
pub struct Semaphore {
    // The number of available permits or the back of the queue (without next edges).
    acquire: Atomic<AcquireState>,
    // A number of releasable permits, and the state of the current release lock.
    release: Atomic<ReleaseState>,
    // The front of the queue (with next edges).
    front: UnsafeCell<*const Waiter>,
    // The last node swapped from AcquireState (with next edges).
    middle: UnsafeCell<*const Waiter>,
    // A stack of nodes that are cancelling.
    next_cancel: Atomic<*const Waiter>,
}

/// A [`Future`] returned by [`Semaphore::acquire`] that produces a [`SemaphoreGuard`].
pub struct AcquireFuture<'a>(UnsafeCell<Waiter>, PhantomData<&'a Semaphore>);

/// A [`Future`] returned by [`Semaphore::acquire_arc`] that produces a [`SemaphoreGuardArc`].
pub struct AcquireFutureArc {
    arc: Arc<Semaphore>,
    inner: AcquireFuture<'static>,
}

unsafe impl Sync for Semaphore {}

unsafe impl Send for Semaphore {}

impl UnwindSafe for Semaphore {}

impl RefUnwindSafe for Semaphore {}

unsafe impl<'a> Sync for AcquireFuture<'a> {}

unsafe impl<'a> Send for AcquireFuture<'a> {}

impl<'a> Debug for AcquireFuture<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AcquireFuture").field(&unsafe { self.waiter() }.amount).finish()
    }
}

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

impl<'a> AcquireFuture<'a> {
    unsafe fn waiter(&self) -> &Waiter {
        &*self.0.get()
    }
    // Try to acquire or add to queue.
    unsafe fn poll_enter(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<SemaphoreGuard<'a>, AcquireError>> {
        (*self.waiter().semaphore).acquire.transact(|mut acquire| {
            let (available, back) = match *acquire {
                Queued(back) => (0, back),
                Available(available) => {
                    let available = match available.into_usize() {
                        None => {
                            *self.waiter().step.get() = AcquireStep::Done;
                            return Ok(Poll::Ready(Err(AcquireError)));
                        }
                        Some(available) => available,
                    };
                    if self.waiter().amount <= available {
                        *acquire = Available(Permits::new(available - self.waiter().amount));
                        acquire.commit()?;
                        *self.waiter().step.get() = AcquireStep::Done;
                        return Ok(Poll::Ready(Ok(SemaphoreGuard::new(
                            &*self.waiter().semaphore, self.waiter().amount))));
                    } else {
                        (available, null())
                    }
                }
            };
            assert!(self.waiter().waker.poll(cx).is_pending());
            *self.waiter().prev.get() = back;
            *acquire = Queued(self.0.get());
            acquire.commit()?;
            *self.waiter().step.get() = AcquireStep::Waiting;
            // Even if available==0, this is necessary to set release to LockedDirty.
            (*self.waiter().semaphore).release(available);
            Ok(Poll::Pending)
        })
    }

    unsafe fn poll_waiting(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<SemaphoreGuard<'a>, AcquireError>> {
        match self.waiter().waker.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(poisoned) => {
                *self.waiter().step.get() = AcquireStep::Done;
                if poisoned {
                    Poll::Ready(Err(AcquireError))
                } else {
                    Poll::Ready(Ok(SemaphoreGuard::new(&*self.waiter().semaphore, self.waiter().amount)))
                }
            }
        }
    }
}

impl<'a> Future for AcquireFuture<'a> {
    type Output = Result<SemaphoreGuard<'a>, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match *(*self.0.get()).step.get() {
                AcquireStep::Entering => {
                    self.poll_enter(cx)
                }
                AcquireStep::Waiting => {
                    self.poll_waiting(cx)
                }
                AcquireStep::Done => panic!("Polling completed future.")
            }
        }
    }
}

impl Future for AcquireFutureArc {
    type Output = Result<SemaphoreGuardArc, AcquireError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match Pin::new_unchecked(&mut self.inner).poll(cx) {
                Poll::Ready(guard) => {
                    let result =
                        SemaphoreGuardArc::new(self.arc.clone(), guard?.forget());
                    Poll::Ready(Ok(result))
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

impl<'a> Drop for AcquireFuture<'a> {
    fn drop(&mut self) {
        unsafe {
            match *self.waiter().step.get() {
                AcquireStep::Waiting => {
                    // Decide whether the finish or cancel wins if there is a race.
                    match self.waiter().waker.start_cancel() {
                        WakerResult::Cancelling => {
                            // Push onto the cancel queue.
                            (*self.waiter().semaphore).next_cancel.transact(|mut next_cancel| {
                                *self.waiter().next_cancel.get() = *next_cancel;
                                *next_cancel = self.0.get();
                                next_cancel.commit()?;
                                Ok(())
                            });
                            // Ensure a flush of the cancel queue is completed or at least scheduled.
                            (*self.waiter().semaphore).release(0);
                            // Wait for a notification that the node can be dropped
                            self.waiter().waker.wait_cancel();
                        }
                        WakerResult::Finished { poisoned } => {
                            // The acquire finished before it could be cancelled. Pretend like
                            // nothing happened and release the acquired permits.
                            if !poisoned {
                                (*self.waiter().semaphore).release(self.waiter().amount);
                            }
                        }
                    }
                }
                AcquireStep::Entering { .. } => {}
                AcquireStep::Done => {}
            }
        }
    }
}

// The state of a single call to release.
struct ReleaseAction<'a> {
    sem: &'a Semaphore,
    releasable: Permits,
}

impl<'a> ReleaseAction<'a> {
    // Attempt to acquire the release lock. If it is locked, defer the release for the lock owner
    // by including the new permits in ReleaseState.
    pub unsafe fn lock_or_else_defer(&mut self) -> bool {
        self.sem.release.transact(|mut release| {
            Ok(match *release {
                Unlocked(available) => {
                    *release = Locked;
                    release.commit()?;
                    self.releasable += available;
                    true
                }
                Locked => {
                    *release = LockedDirty(self.releasable);
                    release.commit()?;
                    false
                }
                LockedDirty(available) => {
                    *release = LockedDirty(available + self.releasable);
                    release.commit()?;
                    false
                }
            })
        })
    }
    // If the queue is empty, make the permits available in AcquireState. Otherwise take all nodes
    // in AcquireState and add next edges.
    unsafe fn flip(&mut self) {
        self.sem.acquire.transact(|mut acquire| {
            Ok(match *acquire {
                Queued(back) if back == null() && *self.sem.front.get() == null() => {
                    *acquire = Available(self.releasable);
                    acquire.commit()?;
                    self.releasable = Permits::new(0);
                }
                Available(available) => {
                    assert!(*self.sem.front.get() == null());
                    *acquire = Available(available + self.releasable);
                    acquire.commit()?;
                    self.releasable = Permits::new(0);
                }
                Queued(back) => {
                    if back != null() {
                        *acquire = Queued(null());
                        acquire.commit()?;
                        let mut waiter = back;
                        while waiter != null() {
                            let prev = *(*waiter).prev.get();
                            if prev == null() {
                                let middle = *self.sem.middle.get();
                                if middle == null() {
                                    *self.sem.front.get() = waiter;
                                    *self.sem.middle.get() = waiter;
                                } else {
                                    *(*middle).next.get() = waiter;
                                    *(*waiter).prev.get() = middle;
                                }
                                break;
                            }
                            *(*prev).next.get() = waiter;
                            waiter = prev;
                        }
                        *self.sem.middle.get() = back;
                    }
                }
            })
        })
    }

    // Clear the cancellation queue, removing nodes from the finish queue. Nodes should have next edges.
    unsafe fn cancel(&mut self, mut next_cancel: *const Waiter) {
        while next_cancel != null() {
            let next = *(*next_cancel).next.get();
            let prev = *(*next_cancel).prev.get();
            if prev == null() {
                *self.sem.front.get() = next;
            } else {
                *(*prev).next.get() = next;
            }
            if next == null() {
                *self.sem.middle.get() = prev;
            } else {
                *(*next).prev.get() = prev;
            }
            let waker = &(*next_cancel).waker as *const AtomicWaker;
            next_cancel = *(*next_cancel).next_cancel.get();
            AtomicWaker::accept_cancel(waker);
        }
    }

    // Try to finish and pop the next node.
    unsafe fn try_pop(&mut self) -> bool {
        let front = *self.sem.front.get();
        let amount = (*front).amount;
        let next = *(*front).next.get();
        let releasable = self.releasable.into_usize();
        if let Some(releasable) = releasable{
            if releasable < (*front).amount {
                return false;
            }
        }
        match AtomicWaker::finish(&(*front).waker, releasable.is_none()) {
            WakerResult::Cancelling =>
                return false,
            WakerResult::Finished { .. } => {
                if let Some(releasable) = releasable {
                    self.releasable = Permits::new(releasable - amount);
                }
                *self.sem.front.get() = next;
                if next == null() {
                    *self.sem.middle.get() = null();
                } else {
                    *(*next).prev.get() = null();
                }
                return true;
            }
        }
    }

    // Attempt to unlock the release lock. If there were concurrent releases or cancels, keep the
    // lock in order to complete those operations.
    unsafe fn try_unlock(&mut self) -> bool {
        self.sem.release.transact(|mut release| {
            Ok(match *release {
                LockedDirty(available) => {
                    *release = Locked;
                    release.commit()?;
                    self.releasable += available;
                    false
                }
                Unlocked(_) => unreachable!(),
                Locked => {
                    *release = Unlocked(self.releasable);
                    release.commit()?;
                    true
                }
            })
        })
    }

    // Perform a release operation
    unsafe fn release(&mut self) {
        if !self.lock_or_else_defer() {
            return;
        }
        loop {
            // Take the cancel stack first to ensure all cancelled nodes have next edges.
            let next_cancel = self.sem.next_cancel.swap(null(), AcqRel);
            // Add next edges
            self.flip();
            // Clear the cancellation queue.
            self.cancel(next_cancel);
            // Finish nodes until stuck.
            while *self.sem.front.get() != null() && self.try_pop() {}
            // Unlock if there were no concurrent operations.
            if self.try_unlock() {
                return;
            }
        }
    }
}

impl Semaphore {
    /// The maximum number of permits that can be made available. This is slightly smaller than
    /// [`usize::MAX`].
    pub const MAX_AVAILABLE: usize = (1 << (size_of::<usize>() * 8 - 3)) - 1;

    /// Create a new semaphore with an initial number of permits.
    pub fn new(initial: usize) -> Self {
        Semaphore {
            acquire: Atomic::new(Available(Permits::new(initial))),
            release: Atomic::new(Unlocked(Permits::new(0))),
            front: UnsafeCell::new(null()),
            middle: UnsafeCell::new(null()),
            next_cancel: Atomic::new(null()),
        }
    }

    /// Wait until there are no older pending calls to `acquire` and at least `amount` permits available.
    /// Then consume the requested permits and return a `SemaphoreGuard`.
    pub fn acquire(&self, amount: usize) -> AcquireFuture {
        AcquireFuture(UnsafeCell::new(Waiter {
            semaphore: self,
            step: UnsafeCell::new(AcquireStep::Entering),
            waker: unsafe { AtomicWaker::new() },
            amount,
            next: UnsafeCell::new(null()),
            prev: UnsafeCell::new(null()),
            next_cancel: UnsafeCell::new(null()),

        }), PhantomData)
    }

    /// Like `acquire`, but fails if the call would block.
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

    /// Like `acquire`, but takes an `Arc<Semaphore>` and returns a guard that is `'static`, `Send` and `Sync`.
    pub fn acquire_arc(self: &Arc<Self>, amount: usize) -> AcquireFutureArc {
        AcquireFutureArc {
            arc: self.clone(),
            inner: unsafe { mem::transmute::<AcquireFuture, AcquireFuture>(self.acquire(amount)) },
        }
    }


    /// Like `try_acquire`, but fails if the call would block, takes an `Arc<Semaphore>`, and returns
    /// a guard that is `'static`, `Send` and `Sync`.
    pub fn try_acquire_arc(self: &Arc<Self>, amount: usize) -> Result<SemaphoreGuardArc, TryAcquireError> {
        let guard = self.try_acquire(amount)?;
        let result = SemaphoreGuardArc::new(self.clone(), amount);
        guard.forget();
        Ok(result)
    }

    /// Return `amount` permits to the semaphore. This will eventually wake any calls to `acquire`
    /// that can succeed with the additional permits.
    pub fn release(&self, amount: usize) {
        unsafe {
            ReleaseAction { sem: self, releasable: Permits::new(amount) }.release();
        }
    }

    /// Poison the semaphore, causing all pending and future acquires to fail.
    pub fn poison(&self) {
        unsafe {
            ReleaseAction { sem: self, releasable: Permits::poison() }.release();
        }
    }
}

