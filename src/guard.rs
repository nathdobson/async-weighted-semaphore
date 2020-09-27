use std::mem;
use crate::Semaphore;
use std::sync::Arc;

/// A guard returned by `acquire` that will `release` the acquired permits when `Drop`ped. This
/// contains a reference to the original semaphore, so it is not `'static`.
pub struct SemaphoreGuard<'a> {
    semaphore: &'a Semaphore,
    amount: usize,
}

/// A guard returned by `acquire` that will `release` the acquired permits when `Drop`ped. This
/// contains an Arc<Semaphore>, so it is `'static', `Send` and `Sync`.
pub struct SemaphoreGuardArc {
    semaphore: Option<Arc<Semaphore>>,
    amount: usize,
}

impl<'a> SemaphoreGuard<'a> {
    pub fn new(semaphore: &'a Semaphore, amount: usize) -> Self {
        SemaphoreGuard { semaphore, amount }
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
        SemaphoreGuardArc { semaphore: Some(semaphore), amount }
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
        self.semaphore.release(self.amount);
    }
}


impl Drop for SemaphoreGuardArc {
    fn drop(&mut self) {
        if let Some(semaphore) = self.semaphore.take() {
            semaphore.release(self.amount);
        }
    }
}