use std::mem;
use crate::Semaphore;
use std::sync::Arc;
use std::thread::panicking;

/// A guard returned by [`Semaphore::acquire`] that will call [`Semaphore::release`] when it
/// is dropped (falls out of scope).
#[must_use]
pub struct SemaphoreGuard<'a> {
    semaphore: &'a Semaphore,
    amount: usize,
    panicking: bool,
}

/// A guard returned by [`Semaphore::acquire_arc`] that will call [`Semaphore::release`] when it
/// is dropped (falls out of scope). Can be sent between threads.
#[must_use]
pub struct SemaphoreGuardArc {
    semaphore: Option<Arc<Semaphore>>,
    amount: usize,
    panicking: bool,
}

impl<'a> SemaphoreGuard<'a> {
    pub fn new(semaphore: &'a Semaphore, amount: usize) -> Self {
        SemaphoreGuard { semaphore, amount, panicking: panicking() }
    }
    /// Drop the guard without `release`ing. This is useful when `release`s don't correspond
    /// one-to-one with `acquires` or it's difficult to send the guard to the releaser.
    pub fn forget(self) -> usize {
        let amount = self.amount;
        mem::forget(self);
        amount
    }
}


impl SemaphoreGuardArc {
    pub fn new(semaphore: Arc<Semaphore>, amount: usize) -> Self {
        SemaphoreGuardArc { semaphore: Some(semaphore), amount, panicking: panicking() }
    }
    /// Drop the guard without `release`ing. This is useful when `release`s don't correspond
    /// one-to-one with `acquires` or it's difficult to send the guard to the releaser.
    pub fn forget(mut self) -> usize {
        self.semaphore = None;
        let amount = self.amount;
        mem::forget(self);
        amount
    }
}


impl<'a> Drop for SemaphoreGuard<'a> {
    fn drop(&mut self) {
        if !self.panicking && panicking() {
            self.semaphore.poison();
        } else {
            self.semaphore.release(self.amount);
        }
    }
}


impl Drop for SemaphoreGuardArc {
    fn drop(&mut self) {
        if let Some(semaphore) = self.semaphore.take() {
            if !self.panicking && panicking() {
                semaphore.poison();
            } else {
                semaphore.release(self.amount);
            }
        }
    }
}