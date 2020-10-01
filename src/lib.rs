//! An async weighted semaphore.
#![allow(unused_imports)]

mod atomic;
mod waker;
mod guard;
mod state;
#[cfg(test)]
mod tests;

use crate::state::DebugPtr;
use std::fmt::Display;
use std::error::Error;
use std::future::Future;
use std::task::{Poll, Context};
use std::pin::Pin;
use std::{fmt, mem};
use std::cell::UnsafeCell;
use std::ptr::{null};
use std::fmt::{Debug, Formatter};
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use crate::state::AcquireState::{Available, Queued};
use crate::state::ReleaseState::{Locked, Unlocked, LockedDirty};
use crate::atomic::{Atomic};
use std::sync::Arc;
pub use crate::guard::SemaphoreGuard;
pub use crate::guard::SemaphoreGuardArc;
use crate::state::{AcquireState, ReleaseState, Permits};
use std::mem::size_of;
use crate::waker::{Waiter, CancelResult, FinishResult};
use std::panic::{UnwindSafe, RefUnwindSafe};

/// An error returned from the `acquire` method on `Semaphore`.
/// This error indicates that the `Semaphore` is poisoned.
#[derive(Debug, Eq, Ord, PartialOrd, PartialEq, Clone, Copy)]
pub struct AcquireError;

impl Error for AcquireError {}

impl Display for AcquireError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// An error returned from the `try_acquire` method on `Semaphore`.
#[derive(Debug, Eq, Ord, PartialOrd, PartialEq)]
pub enum TryAcquireError {
    /// There is insufficient capacity or a call to `acquire` is blocked, so it is not possible
    /// to acquire without blocking.
    WouldBlock,
    /// The call to `try_acquire` failed because the `Semaphore` is poisoned.
    Poisoned,
}

impl Error for TryAcquireError {}

impl Display for TryAcquireError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// An async weighted semaphore.
/// A `Semaphore` is a synchronization primitive for limiting concurrent usage of a resource.
///
/// A `Semaphore` starts with an initial number of permits available. When a client `acquire`s a number
/// of permits, the permits become unavailable. If a client attempts to `acquire` more permits
/// than are available, the call will block. With the permits `acquire`d, the client may safely
/// use that amount of the shared resource. When the client is done with the resource, it should
/// `release` the the permits to make them available.
///
/// # Priority Policy
/// `acquire` provides FIFO semantics: calls to `acquire` finish in the same order that
/// they start. If there is a pending call to `acquire`, a new call to `acquire` will always block,
/// even if there are enough permits available for the new call. This policy reduces starvation and
/// tail latency at the cost of utilization.
/// ```
/// use async_weighted_semaphore::Semaphore;
/// use futures::executor::LocalPool;
/// use futures::task::{LocalSpawnExt, SpawnExt};
/// use std::rc::Rc;
///
/// let sem = Rc::new(Semaphore::new(1));
/// let mut pool = LocalPool::new();
/// let spawn = pool.spawner();
/// // Begin a call to acquire(2).
/// spawn.spawn_local({let sem = sem.clone(); async move{ sem.acquire(2).await.unwrap().forget(); }});
/// // Ensure the call to acquire(2) has started.
/// pool.run_until_stalled();
/// // There is 1 permit available, but it cannot be acquired.
/// assert!(sem.try_acquire(1).is_err());
/// ```
///
/// # Performance
/// This implementation is wait-free except for the use of dynamic allocation.
/// # Poisoning
/// If a guard is dropped while panicking, or the number of available permits exceeds `MAX_AVAILABLE`,
/// the semaphore will be permanently poisoned. All current and future acquires will fail immediately,
/// and release will become a no-op.
pub struct Semaphore {
    // The nominal state of this semaphore is a queue of Waiters, a counter of available permits,
    // and a release lock guarding the process of popping Waiters off the queue and waking them.
    // The counter of available permits is stored by acquire, release, and the release lock owner.
    // The queue is stored by a pair of stacks is opposing directions. The front is guarded by the
    // release lock. The back is encoded in acquire if present.

    // The information needed to acquire.
    acquire: Atomic<AcquireState>,

    // The state of release operations.
    release: Atomic<ReleaseState>,

    // The front of the queue (owned by the release lock owner).
    front: UnsafeCell<*const Waiter<WaiterData>>,
}

struct WaiterData {
    // The next waiter in the current stack (may be before or after in the queue).
    next: UnsafeCell<*const Waiter<WaiterData>>,
    amount: UnsafeCell<usize>,
    poisoned: UnsafeCell<bool>,
}

#[derive(Debug)]
enum AcquireStep {
    Enter,
    Loop(*const Waiter<WaiterData>),
    Done,
}

/// A `Future` that `acquire`s a specified number of permits from a `Semaphore` and returns a `SemaphoreGuard`.
pub struct AcquireFuture<'a> {
    semaphore: &'a Semaphore,
    amount: usize,
    step: AcquireStep,
}

/// A `Future` that `acquire`s a specified number of permits from an `Arc<Semaphore>` and returns a `SemaphoreGuardArc`.
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

impl<'a> Unpin for AcquireFuture<'a> {}


impl<'a> Debug for AcquireFuture<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcquireFuture")
            .field("amount", &self.amount)
            .field("step", &self.step)
            .finish()
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        unsafe {
            let back = match self.acquire.load(Relaxed) {
                Queued(back) => back,
                Available(_) => null(),
            };
            for &(mut ptr) in &[*self.front.get(), back] {
                while ptr != null() {
                    let next = *(*ptr).next.get();
                    (*ptr).free();
                    ptr = next;
                }
            }
        }
    }
}

impl Debug for Semaphore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        unsafe {
            let mut w = f.debug_struct("Semaphore");
            w.field("acquire", &self.acquire.load(SeqCst));
            w.field("release", &self.release.load(SeqCst));
            w.field("front", &DebugPtr(*self.front.get()));
            w.finish()?;
            Ok(())
        }
    }
}

impl<'a> AcquireFuture<'a> {
    unsafe fn poll_enter(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<SemaphoreGuard<'a>, AcquireError>> {
        let mut waiter: *const Waiter<WaiterData> = null();
        self.semaphore.acquire.transact(|mut acquire| {
            let (available, back) = match *acquire {
                Queued(back) => (0, back),
                Available(available) => {
                    let available = match available.into_usize() {
                        None => {
                            self.step = AcquireStep::Done;
                            if waiter != null() {
                                (*waiter).free();
                                waiter = null();
                            }
                            return Ok(Poll::Ready(Err(AcquireError)));
                        }
                        Some(available) => available,
                    };
                    if self.amount <= available {
                        *acquire = Available(Permits::new(available - self.amount));
                        acquire.commit()?;
                        if waiter != null() {
                            (*waiter).free();
                            waiter = null();
                        }
                        self.step = AcquireStep::Done;
                        return Ok(Poll::Ready(Ok(SemaphoreGuard::new(self.semaphore, self.amount))));
                    } else {
                        (available, null())
                    }
                }
            };
            if waiter == null() {
                waiter = Waiter::new(cx, WaiterData {
                    next: UnsafeCell::new(null()),
                    amount: UnsafeCell::new(0),
                    poisoned: UnsafeCell::new(false),
                });
            }
            *(*waiter).next.get() = back;
            *(*waiter).amount.get() = self.amount;
            *acquire = Queued(waiter);
            acquire.commit()?;
            self.step = AcquireStep::Loop(waiter);
            self.semaphore.release(available);
            Ok(Poll::Pending)
        })
    }
}

impl<'a> Future for AcquireFuture<'a> {
    type Output = Result<SemaphoreGuard<'a>, AcquireError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match self.step {
                AcquireStep::Enter => {
                    self.poll_enter(cx)
                }
                AcquireStep::Loop(waiter) => {
                    match (*waiter).poll(cx) {
                        Poll::Pending => Poll::Pending,
                        Poll::Ready(data) => {
                            self.step = AcquireStep::Done;
                            if *data.poisoned.get() {
                                Poll::Ready(Err(AcquireError))
                            } else {
                                Poll::Ready(Ok(SemaphoreGuard::new(self.semaphore, self.amount)))
                            }
                        }
                    }
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

impl Unpin for AcquireFutureArc {}

impl<'a> Drop for AcquireFuture<'a> {
    fn drop(&mut self) {
        unsafe {
            match self.step {
                AcquireStep::Loop(waiter) => {
                    match (*waiter).cancel() {
                        CancelResult::Cancelled => {
                            self.semaphore.release(0);
                        }
                        CancelResult::Finished(data) => {
                            self.semaphore.release(*data.amount.get());
                        }
                    }
                }
                AcquireStep::Enter { .. } => {}
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
                Queued(back) if back == null() => {
                    *acquire = Available(self.releasable);
                    acquire.commit()?;
                    self.releasable = Permits::new(0);
                }
                Available(available) => {
                    *acquire = Available(available + self.releasable);
                    acquire.commit()?;
                    self.releasable = Permits::new(0);
                }
                Queued(mut back) => {
                    *acquire = Queued(null());
                    acquire.commit()?;
                    while back != null() {
                        let next = *(*back).next.get();
                        *(*back).next.get() = *self.sem.front.get();
                        *self.sem.front.get() = back;
                        back = next;
                    }
                }
            })
        })
    }

    unsafe fn try_pop(&mut self) -> bool {
        let front = *self.sem.front.get();
        let amount = *(*front).amount.get();
        if let Some(releasable) = self.releasable.into_usize() {
            if amount <= releasable {
                *self.sem.front.get() = *(*front).next.get();
                match (*front).finish() {
                    FinishResult::Cancelled => {
                        return true;
                    }
                    FinishResult::Finished => {
                        self.releasable = Permits::new(releasable - amount);
                        return true;
                    }
                }
            }
        } else {
            *self.sem.front.get() = *(*front).next.get();
            *(*front).poisoned.get() = true;
            (*front).finish();
            return true;
        }
        let next = *(*front).next.get();
        if (*front).check_cancelled() {
            *self.sem.front.get() = next;
            return true;
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
            if *self.sem.front.get() == null() {
                // If the front is empty and there are waiters at the back of the queue, move them
                // to the front. Otherwise make the permits acquirable.
                self.flip();
            }
            if *self.sem.front.get() != null() {
                // Try to complete one acquire and pop it from the queue.
                if self.try_pop() {
                    continue;
                }
            }
            // Attempt to lose the release lock. If there are additional releasable permits, take
            // those instead of unlocking.
            if self.try_unlock() {
                return;
            }
        }
    }
}

impl Semaphore {
    pub const MAX_AVAILABLE: usize = (1 << (size_of::<usize>() * 8 - 3)) - 1;

    /// Create a new semaphore with an initial number of permits.
    pub fn new(initial: usize) -> Self {
        Semaphore {
            acquire: Atomic::new(Available(Permits::new(initial))),
            release: Atomic::new(Unlocked(Permits::new(0))),
            front: UnsafeCell::new(null()),
        }
    }

    /// Wait until there are no older pending calls to `acquire` and at least `amount` permits available.
    /// Then consume the requested permits and return a `SemaphoreGuard`.
    pub fn acquire(&self, amount: usize) -> AcquireFuture {
        AcquireFuture {
            semaphore: self,
            amount,
            step: AcquireStep::Enter,
        }
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

