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
    semaphore: Option<Arc<Semaphore>>,
    amount: usize,
    panicking: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemaphoreGuardSplitErr {
    Underflow
}

impl<'a> SemaphoreGuard<'a> {
    pub fn new(semaphore: &'a Semaphore, amount: usize) -> Self {
        SemaphoreGuard { semaphore, amount, panicking: panicking() }
    }

    /// Combine two `SemaphoreGuard`s into one, with the sum of the originals' permits.
    ///
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::Semaphore;
    /// # tokio_test::block_on(async {
    /// let semaphore = Semaphore::new(15);
    /// let mut g1 = semaphore.acquire(10).await.unwrap();
    /// let g2 = semaphore.acquire(5).await.unwrap();
    /// g1.extend(g2);
    /// # })
    /// ```
    pub fn extend(&mut self, other: SemaphoreGuard<'a>) {
        assert!(std::ptr::eq(self.semaphore, other.semaphore),
            "Can't extend a guard with a guard from a different Semaphore");
        self.amount = self.amount.saturating_add(other.forget());
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

    /// Split this `SemaphoreGuard` into two.
    ///
    /// The new guard will have `permits` permits, and this guard's permits will be reduced
    /// accordingly.
    ///
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::Semaphore;
    /// # tokio_test::block_on(async {
    /// let semaphore = Semaphore::new(15);
    /// let mut g1 = semaphore.acquire(15).await.unwrap();
    /// let g2 = g1.split(5).unwrap();
    /// # })
    /// ```
    pub fn split(&mut self, permits: usize) -> Result<SemaphoreGuard<'a>, SemaphoreGuardSplitErr> {
        if self.amount >= permits {
            self.amount -= permits;
            Ok(SemaphoreGuard {
                semaphore: self.semaphore.clone(),
                amount: permits,
                panicking: self.panicking
            })
        } else {
            Err(SemaphoreGuardSplitErr::Underflow)
        }
    }
}

impl SemaphoreGuardArc {
    pub fn new(semaphore: Arc<Semaphore>, amount: usize) -> Self {
        SemaphoreGuardArc { semaphore: Some(semaphore), amount, panicking: panicking() }
    }

    /// Combine two `SemaphoreGuardArc`s into one, with the sum of the originals' permits.
    ///
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::Semaphore;
    /// # tokio_test::block_on(async {
    /// let semaphore = Semaphore::new(15);
    /// let mut g1 = semaphore.acquire(10).await.unwrap();
    /// let g2 = semaphore.acquire(5).await.unwrap();
    /// g1.extend(g2);
    /// # })
    /// ```
    pub fn extend(&mut self, other: SemaphoreGuardArc) {
        let sem1 = self.semaphore.as_ref().unwrap();
        let sem2 = other.semaphore.as_ref().unwrap();
        assert!(Arc::ptr_eq(sem1, sem2),
            "Can't extend a guard with a guard from a different Semaphore");
        self.amount = self.amount.saturating_add(other.forget());
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
    pub fn forget(mut self) -> usize {
        self.semaphore = None;
        let amount = self.amount;
        mem::forget(self);
        amount
    }

    /// Split this `SemaphoreGuardArc` into two.
    ///
    /// The new guard will have `permits` permits, and this guard's permits will be reduced
    /// accordingly.
    ///
    /// # Examples
    /// ```
    /// # use async_weighted_semaphore::Semaphore;
    /// # use std::sync::Arc;
    /// # tokio_test::block_on(async {
    /// let semaphore = Arc::new(Semaphore::new(15));
    /// let mut g1 = semaphore.acquire_arc(15).await.unwrap();
    /// let g2 = g1.split(5).unwrap();
    /// # })
    /// ```
    pub fn split(&mut self, permits: usize) -> Result<SemaphoreGuardArc, SemaphoreGuardSplitErr> {
        if self.amount >= permits {
            self.amount -= permits;
            Ok(SemaphoreGuardArc {
                semaphore: self.semaphore.clone(),
                amount: permits,
                panicking: self.panicking
            })
        } else {
            Err(SemaphoreGuardSplitErr::Underflow)
        }
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

