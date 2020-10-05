use std::fmt;
use std::fmt::Display;
use std::error::Error;
#[allow(unused_imports)] // for doc links
use crate::Semaphore;

/// An error returned by [`Semaphore::acquire`] and [`Semaphore::acquire_arc`] to indicate the
/// Semaphore has been poisoned. See [`Semaphore::acquire_arc`] for an example usage.
#[derive(Debug, Eq, Ord, PartialOrd, PartialEq, Clone, Copy, Hash, Default)]
pub struct PoisonError;

impl Error for PoisonError {}

impl Display for PoisonError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> fmt::Result {
        Display::fmt("the semaphore has been poisoned", f)
    }
}

/// An error returned from [`Semaphore::try_acquire`] and [`Semaphore::try_acquire_arc`]. See
/// [`Semaphore::try_acquire_arc`] for an example usage.
#[derive(Debug, Eq, Ord, PartialOrd, PartialEq, Clone, Copy, Hash)]
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
        match self {
            TryAcquireError::WouldBlock => Display::fmt("the call to acquire would have blocked", f),
            TryAcquireError::Poisoned => Display::fmt(&PoisonError, f),
        }
    }
}

impl From<PoisonError> for TryAcquireError {
    fn from(_: PoisonError) -> Self {
        TryAcquireError::Poisoned
    }
}