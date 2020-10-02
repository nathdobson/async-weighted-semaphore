//! An asynchronous weighted [`Semaphore`].
#![allow(unused_imports)]

mod atomic;
mod waker;
mod guard;
mod state;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[macro_use]
extern crate lazy_static;


use crate::state::{DebugPtr, Waiter};
use std::fmt::Display;
use std::error::Error;
use std::future::Future;
use std::task::{Poll, Context};
use std::pin::Pin;
use std::{fmt, mem, thread};
use std::cell::UnsafeCell;
use std::ptr::{null};
use std::fmt::{Debug, Formatter};
use std::sync::atomic::Ordering::{Relaxed, SeqCst, AcqRel};
use crate::state::AcquireState::{Available, Queued};
use crate::state::ReleaseState::{Locked, Unlocked, LockedDirty};
use crate::atomic::{Atomic};
use std::sync::Arc;
pub use crate::guard::SemaphoreGuard;
pub use crate::guard::SemaphoreGuardArc;
use crate::state::{AcquireState, ReleaseState, Permits};
use std::mem::size_of;
use std::panic::{UnwindSafe, RefUnwindSafe};
use std::marker::PhantomData;
use crate::state::AcquireStep;
use crate::waker::{AtomicWaker, CancelResult, FinishResult};

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
pub struct Semaphore {
    acquire: Atomic<AcquireState>,
    release: Atomic<ReleaseState>,
    front: UnsafeCell<*const Waiter>,
    middle: UnsafeCell<*const Waiter>,
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
        write!(f, "{:?}", unsafe { &*self.0.get() })
    }
}

impl Debug for Semaphore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        unsafe {
            let mut w = f.debug_struct("Semaphore");
            w.field("acquire", &self.acquire.load(SeqCst));
            w.field("release", &self.release.load(SeqCst));
            w.field("front", &DebugPtr(*self.front.get()));
            w.field("middle", &DebugPtr(*self.middle.get()));
            w.finish()?;
            Ok(())
        }
    }
}

impl<'a> AcquireFuture<'a> {
    unsafe fn waiter(&self) -> &Waiter {
        &*self.0.get()
    }
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
            // Even if available==0, this is necessary to mark the dirty bit.
            (*self.waiter().semaphore).release(available);
            Ok(Poll::Pending)
        })
    }
    unsafe fn poll_waiting(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<SemaphoreGuard<'a>, AcquireError>> {
        match self.waiter().waker.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(poisoned) => {
                //println!("Ready {:?}", self.0.get());
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
                    //println!("Cancelling {:?}", self.0.get());
                    match self.waiter().waker.start_cancel() {
                        CancelResult::Cancelling => {
                            (*self.waiter().semaphore).next_cancel.transact(|mut next_cancel| {
                                *self.waiter().next_cancel.get() = *next_cancel;
                                *next_cancel = self.0.get();
                                next_cancel.commit()?;
                                Ok(())
                            });
                            (*self.waiter().semaphore).release(0);
                            self.waiter().waker.wait_cancel();
                        }
                        CancelResult::Finished { poisoned } => {
                            if !poisoned {
                                (*self.waiter().semaphore).release(self.waiter().amount);
                            }
                        }
                    }
                }
                AcquireStep::Entering { .. } => {}
                AcquireStep::Done => {}
            }
            //println!("Dropping {:?} {:?}", self.0.get(), *self.waiter().step.get());
        }
    }
}

// The state of a single call to release.
struct ReleaseAction<'a> {
    sem: &'a Semaphore,
    releasable: Permits,
}

impl<'a> ReleaseAction<'a> {
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

    unsafe fn try_pop(&mut self) -> bool {
        let front = *self.sem.front.get();
        if let Some(releasable) = self.releasable.into_usize() {
            if (*front).amount <= releasable {
                let amount = (*front).amount;
                let next = *(*front).next.get();
                //println!("Finishing {:?}", front);
                match AtomicWaker::finish(&(*front).waker, false) {
                    FinishResult::Cancelling => {
                        return false;
                    }
                    FinishResult::Finished { .. } => {
                        self.releasable = Permits::new(releasable - amount);
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
        } else {
            let next = *(*front).next.get();
            //println!("Poisoning {:?}", front);
            match AtomicWaker::finish(&(*front).waker, true) {
                FinishResult::Cancelling => {
                    return false;
                }
                FinishResult::Finished { .. } => {
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
        false
    }

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

    unsafe fn release(&mut self) {
        // Try to get a release lock. If it's already locked, defer these permits to the other
        // releaser.
        if !self.lock_or_else_defer() {
            return;
        }
        loop {
            //println!("");
            //println!("A {:?}",self.sem);
            let next_cancel = self.sem.next_cancel.swap(null(), AcqRel);
            //println!("B {:?}",self.sem);
            self.flip();
            //println!("C {:?}",self.sem);
            self.cancel(next_cancel);
            //println!("D {:?}",self.sem);
            while *self.sem.front.get() != null() && self.try_pop() {}
            //println!("E {:?}",self.sem);
            // Attempt to lose the release lock. If there are additional releasable permits, take
            // those instead of unlocking.
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

