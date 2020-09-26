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
pub use crate::guard::SemaphoreGuardWith;
use crate::state::{ReleaseMode, AcquireState, ReleaseState};


#[derive(Debug, Eq, Ord, PartialOrd, PartialEq, Clone, Copy)]
pub struct AcquireError;

impl Error for AcquireError {}

impl Display for AcquireError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Eq, Ord, PartialOrd, PartialEq)]
pub enum TryAcquireError {
    WouldBlock,
    Shutdown,
}

impl Error for TryAcquireError {}

impl Display for TryAcquireError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub struct Semaphore {
    acquire: Atomic<AcquireState>,
    release: Atomic<ReleaseState>,
    front: UnsafeCell<*const Waiter>,
}

#[repr(align(64))]
pub struct Waiter {
    waker: AtomicWaker,
    next: UnsafeCell<*const Waiter>,
    remaining: UnsafeCell<usize>,
}

pub enum AcquireStep {
    Enter,
    Loop(*const Waiter),
    Poison,
}

pub struct AcquireFuture<'a> {
    semaphore: &'a Semaphore,
    amount: usize,
    step: AcquireStep,
}

pub struct AcquireArcFuture<'a> {
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

impl<'a> Future for AcquireArcFuture<'a> {
    type Output = SemaphoreGuardWith<Arc<Semaphore>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match Pin::new_unchecked(&mut self.inner).poll(cx) {
                Poll::Ready(guard) => {
                    let result =
                        SemaphoreGuardWith::new(self.arc.clone(), guard.forget());
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


    pub fn acquire(&self, amount: usize) -> AcquireFuture {
        AcquireFuture {
            semaphore: self,
            amount,
            step: AcquireStep::Enter,
        }
    }

    pub fn new(initial: usize) -> Self {
        Semaphore {
            acquire: Atomic::new(Available(initial)),
            release: Atomic::new(ReleaseState { releasable: 0, mode: ReleaseMode::Unlocked }),
            front: UnsafeCell::new(null()),
        }
    }

    fn release(&self, amount: usize) {
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

    pub fn acquire_arc<'a>(self: &'a Arc<Self>, amount: usize) -> AcquireArcFuture<'a> {
        AcquireArcFuture { arc: self, inner: self.acquire(amount) }
    }


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

    pub fn try_acquire_arc(self: &Arc<Self>, amount: usize) -> Result<SemaphoreGuardWith<Arc<Self>>, TryAcquireError> {
        let guard = self.try_acquire(amount)?;
        let result = SemaphoreGuardWith::new(self.clone(), amount);
        guard.forget();
        Ok(result)
    }
}

