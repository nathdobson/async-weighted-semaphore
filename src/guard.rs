use std::{mem, fmt};
use crate::Semaphore;
use std::sync::Arc;
use std::thread::panicking;
use std::fmt::{Debug, Formatter};

/// A guard returned by [`Semaphore::acquire`] that will call [`Semaphore::release`] when it
/// is dropped (falls out of scope).
/// # Examples
/// ```
/// # use futures::executor::block_on;
/// use async_weighted_semaphore::{Semaphore, SemaphoreGuard};
/// # block_on(async{
/// let semaphore = Semaphore::new(1);
/// let guard: SemaphoreGuard = semaphore.acquire(1).await.unwrap();
/// # })
/// ```
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
    semaphore: Arc<Semaphore>,
    amount: usize,
    panicking: bool,
}

impl<'a> SemaphoreGuard<'a> {
    pub fn new(semaphore: &'a Semaphore, amount: usize) -> Self {
        SemaphoreGuard { semaphore, amount, panicking: panicking() }
    }
    /// Drop the guard without calling [`Semaphore::release`]. This is useful when `release`s don't
    /// correspond one-to-one with `acquires` or it's difficult to send the guard to the releaser.
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::{Semaphore, PoisonError, SemaphoreGuardArc};
    /// use async_channel::{Sender, SendError};
    /// // Limit size of a producer-consumer queue. Receivers may wait for any number of items
    /// // to be available.
    /// async fn send<T>(semaphore: &Semaphore,
    ///                  sender: &Sender<T>,
    ///                  message: T
    ///         ) -> Result<(), SendError<T>>{
    ///     match semaphore.acquire(1).await {
    ///         // A semaphore can be poisoned to prevent deadlock when a channel closes.
    ///         Err(PoisonError) => Err(SendError(message)),
    ///         Ok(guard) => {
    ///             sender.send(message).await?;
    ///             guard.forget();
    ///             Ok(())
    ///         }
    ///     }
    /// }
    /// ```
    pub fn forget(self) -> usize {
        let amount = self.amount;
        mem::forget(self);
        amount
    }
}

impl SemaphoreGuardArc {
    pub fn new(semaphore: Arc<Semaphore>, amount: usize) -> Self {
        SemaphoreGuardArc { semaphore, amount, panicking: panicking() }
    }
    /// Drop the guard without calling [`Semaphore::release`]. This is useful when `release`s don't
    /// correspond one-to-one with `acquires` or it's difficult to send the guard to the releaser.
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::{Semaphore, PoisonError, SemaphoreGuardArc};
    /// # use std::sync::Arc;
    /// use async_channel::{Sender, SendError};
    /// // Limit size of a producer-consumer queue. Receivers may wait for any number of items
    /// // to be available.
    /// async fn send<T>(semaphore: &Arc<Semaphore>,
    ///                  sender: &Sender<T>,
    ///                  message: T
    ///         ) -> Result<(), SendError<T>>{
    ///     match semaphore.acquire_arc(1).await {
    ///         // A semaphore can be poisoned to prevent deadlock when a channel closes.
    ///         Err(PoisonError) => Err(SendError(message)),
    ///         Ok(guard) => {
    ///             sender.send(message).await?;
    ///             guard.forget();
    ///             Ok(())
    ///         }
    ///     }
    /// }
    /// ```
    pub fn forget(self) -> usize {
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
        if !self.panicking && panicking() {
            self.semaphore.poison();
        } else {
            self.semaphore.release(self.amount);
        }
    }
}

impl<'a> Debug for SemaphoreGuard<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "SemaphoreGuard({})", self.amount)
    }
}

impl Debug for SemaphoreGuardArc {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "SemaphoreGuardArc({})", self.amount)
    }
}

