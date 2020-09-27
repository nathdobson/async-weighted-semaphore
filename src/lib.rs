//! An async weighted semaphore.

mod atomic;
mod waker;
mod guard;
mod state;
#[cfg(test)]
mod tests;


use std::fmt::Display;
use std::error::Error;
use std::future::Future;
use std::task::{Poll, Context};
use std::pin::Pin;
use std::{fmt};
use std::cell::UnsafeCell;
use std::ptr::{null};
use std::fmt::{Debug, Formatter};
use std::sync::atomic::Ordering::{Relaxed, Acquire};
use crate::state::ReleaseMode::{Unlocked, Locked, LockedDirty};
use crate::waker::{AtomicWaker, PollResult, CancelResult, FinishResult};
use crate::state::AcquireState::{Available, Queued};
use crate::atomic::{Atomic};
use std::sync::Arc;
pub use crate::guard::SemaphoreGuard;
use crate::state::{ReleaseMode, AcquireState, ReleaseState};
pub use crate::guard::SemaphoreGuardArc;

/// An error returned from the `acquire` method on `Semaphore`.
/// This error indicates that the `Semaphore` shut down.
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
    /// The call to `try_acquire` failed because the `Semaphore` shut down.
    Shutdown,
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
/// spawn.spawn_local({let sem = sem.clone(); async move{ sem.acquire(2).await.forget(); }});
/// // Ensure the call to acquire(2) has started.
/// pool.run_until_stalled();
/// // There is 1 permit available, but it cannot be acquired.
/// assert!(sem.try_acquire(1).is_err());
/// ```
///
/// # Performance
/// This implementation is wait-free except for the use of dynamic allocation.
pub struct Semaphore {
    acquire: Atomic<AcquireState>,
    release: Atomic<ReleaseState>,
    front: UnsafeCell<*const Waiter>,
}

struct Waiter {
    waker: AtomicWaker,
    next: UnsafeCell<*const Waiter>,
    remaining: UnsafeCell<usize>,
}

enum AcquireStep {
    Enter,
    Loop(*const Waiter),
    Poison,
}

/// A `Future` that `acquire`s a specified number of permits from a `Semaphore` and returns a `SemaphoreGuard`.
pub struct AcquireFuture<'a> {
    semaphore: &'a Semaphore,
    amount: usize,
    step: AcquireStep,
}

/// A `Future` that `acquire`s a specified number of permits from an `Arc<Semaphore>` and returns a `SemaphoreGuardArc`.
pub struct AcquireFutureArc<'a> {
    arc: &'a Arc<Semaphore>,
    inner: AcquireFuture<'a>,
}

unsafe impl Sync for Semaphore {}

unsafe impl Send for Semaphore {}

unsafe impl<'a> Sync for AcquireFuture<'a> {}

unsafe impl<'a> Send for AcquireFuture<'a> {}

struct DebugPtr(*const Waiter);

impl Debug for DebugPtr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        unsafe {
            write!(f, "{:?}", self.0)?;
            if self.0 != null() {
                write!(f, " {:?} {:?}", (*self.0).remaining, DebugPtr(*(*self.0).next.get()))?;
            }
            Ok(())
        }
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
                    Box::from_raw(ptr as *mut Waiter);
                    ptr = next;
                }
            }
        }
    }
}

impl Debug for Semaphore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut w = f.debug_struct("Semaphore");
        w.finish()
    }
}

impl<'a> AcquireFuture<'a> {
    unsafe fn poll_enter(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SemaphoreGuard<'a>> {
        let mut waiter: *const Waiter = null();
        self.semaphore.acquire.transact(|mut acquire| {
            match *acquire {
                Queued(back) => {
                    if waiter == null() {
                        waiter = Box::into_raw(Box::new(Waiter {
                            waker: AtomicWaker::new(cx.waker().clone()),
                            next: UnsafeCell::new(null()),
                            remaining: UnsafeCell::new(0),
                        }));
                    }
                    *(*waiter).next.get() = back;
                    *(*waiter).remaining.get() = self.amount;
                    *acquire = Queued(waiter);
                    acquire.commit()?;
                    self.step = AcquireStep::Loop(waiter);
                    return Ok(Poll::Pending);
                }
                Available(available) => {
                    if self.amount <= available {
                        *acquire = Available(available - self.amount);
                        acquire.commit()?;
                        if waiter != null() {
                            Box::from_raw(waiter as *mut Waiter);
                            waiter = null();
                        }
                        self.step = AcquireStep::Poison;
                        return Ok(Poll::Ready(SemaphoreGuard::new(self.semaphore, self.amount)));
                    } else {
                        if waiter == null() {
                            waiter = Box::into_raw(Box::new(Waiter {
                                waker: AtomicWaker::new(cx.waker().clone()),
                                next: UnsafeCell::new(null()),
                                remaining: UnsafeCell::new(0),
                            }));
                        }
                        *(*waiter).next.get() = null();
                        *(*waiter).remaining.get() = self.amount - available;
                        *acquire = Queued(waiter);
                        acquire.commit()?;
                        self.step = AcquireStep::Loop(waiter);
                        return Ok(Poll::Pending);
                    }
                }
            }
        })
    }
}

impl<'a> Future for AcquireFuture<'a> {
    type Output = SemaphoreGuard<'a>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match self.step {
                AcquireStep::Enter => {
                    self.poll_enter(cx)
                }
                AcquireStep::Loop(waiter) => {
                    match (*waiter).waker.poll(cx.waker()) {
                        PollResult::Pending => Poll::Pending,
                        PollResult::Finished => {
                            self.step = AcquireStep::Poison;
                            Poll::Ready(SemaphoreGuard::new(self.semaphore, self.amount))
                        }
                        PollResult::FinishedFree => {
                            Box::from_raw(waiter as *mut Waiter);
                            self.step = AcquireStep::Poison;
                            Poll::Ready(SemaphoreGuard::new(self.semaphore, self.amount))
                        }
                    }
                }
                AcquireStep::Poison => unreachable!()
            }
        }
    }
}

impl<'a> Future for AcquireFutureArc<'a> {
    type Output = SemaphoreGuardArc;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match Pin::new_unchecked(&mut self.inner).poll(cx) {
                Poll::Ready(guard) => {
                    let result =
                        SemaphoreGuardArc::new(self.arc.clone(), guard.forget());
                    Poll::Ready(result)
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

impl<'a> Drop for AcquireFuture<'a> {
    fn drop(&mut self) {
        unsafe {
            match self.step {
                AcquireStep::Loop(waiter) => {
                    match (*waiter).waker.cancel() {
                        CancelResult::Cancelled => {}
                        CancelResult::FinishedFree => {
                            self.semaphore.release(self.amount);
                            Box::from_raw(waiter as *mut Waiter);
                        }
                    }
                }
                AcquireStep::Enter { .. } => {}
                AcquireStep::Poison => {}
            }
        }
    }
}


impl Semaphore {
    unsafe fn clear_dirty(&self) {
        self.release.transact(|mut release| {
            if release.mode == LockedDirty {
                release.mode = Locked;
                release.commit()?;
            }
            Ok(())
        });
    }

    unsafe fn consume(&self, amount: usize) {
        self.release.transact(|mut release| {
            release.releasable -= amount;
            release.commit()?;
            Ok(())
        })
    }

    unsafe fn flip(&self) {
        let release = self.release.load(Acquire);
        self.acquire.transact(|mut acquire| {
            match *acquire {
                Queued(back) if back == null() => {
                    *acquire = Available(release.releasable);
                    acquire.commit()?;
                    self.consume(release.releasable);
                    Ok(())
                }
                Available(available) => {
                    *acquire = Available(available + release.releasable);
                    acquire.commit()?;
                    self.consume(release.releasable);
                    Ok(())
                }
                Queued(mut back) => {
                    *acquire = Queued(null());
                    acquire.commit()?;
                    while back != null() {
                        let next = *(*back).next.get();
                        *(*back).next.get() = *(*self).front.get();
                        *self.front.get() = back;
                        back = next;
                    }
                    Ok(())
                }
            }
        });
    }

    unsafe fn try_pop(&self) -> bool {
        let front = *self.front.get();
        let remaining = *(*front).remaining.get();
        let release = self.release.load(Acquire);
        if remaining <= release.releasable {
            *self.front.get() = *(*front).next.get();
            match (*front).waker.finish() {
                FinishResult::Cancelled => {
                    Box::from_raw(front as *mut Waiter);
                    return true;
                }
                FinishResult::Finished => {}
                FinishResult::FinishedFree =>
                    { Box::from_raw(front as *mut Waiter); }
            }
            self.release.transact(|mut release| {
                release.releasable -= remaining;
                release.commit()?;
                Ok(())
            });
            return true;
        }
        if (*front).waker.is_cancelled() {
            *self.front.get() = *(*front).next.get();
            Box::from_raw(front as *mut Waiter);
            return true;
        }
        false
    }

    unsafe fn try_unlock(&self) -> bool {
        self.release.transact(|mut release| {
            if release.mode == Locked {
                release.mode = Unlocked;
                release.commit()?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    unsafe fn unlock(&self) {
        loop {
            self.clear_dirty();
            if *self.front.get() == null() {
                self.flip();
            }
            if *self.front.get() != null() {
                if self.try_pop() {
                    continue;
                }
            }
            if self.try_unlock() {
                return;
            }
        }
    }

    /// Create a new semaphore with an initial number of permits.
    pub fn new(initial: usize) -> Self {
        Semaphore {
            acquire: Atomic::new(Available(initial)),
            release: Atomic::new(ReleaseState { releasable: 0, mode: ReleaseMode::Unlocked }),
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
                    if amount <= available {
                        *acquire = Available(available - amount);
                        acquire.commit()?;
                        Ok(SemaphoreGuard::new(self, amount))
                    } else {
                        Err(TryAcquireError::WouldBlock)
                    }
                }
            })
        })
    }

    /// Like `acquire`, but takes an `Arc<Semaphore>` and returns a guard that is `'static`, `Send` and `Sync`.
    pub fn acquire_arc<'a>(self: &'a Arc<Self>, amount: usize) -> AcquireFutureArc<'a> {
        AcquireFutureArc { arc: self, inner: self.acquire(amount) }
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
            self.release.transact(|mut release| {
                release.releasable += amount;
                match release.mode {
                    Unlocked => {
                        release.mode = Locked;
                        release.commit()?;
                        self.unlock();
                        Ok(())
                    }
                    Locked | LockedDirty => {
                        release.mode = LockedDirty;
                        release.commit()?;
                        Ok(())
                    }
                }
            });
        }
    }
}

