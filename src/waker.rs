use std::cell::UnsafeCell;
use std::task::{Waker};
use std::sync::atomic::Ordering::{Acquire};
use crate::waker::Flag::{Sleeping, Storing, Finished, Cancelled, Loading};
use crate::atomic::{Atomic, Packable};
use std::mem;

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
enum Flag {
    // There is a valid waker. The waker lock is not held. No operations are in progress.
    Sleeping,
    // A spurious poll has occurred. The poll holds the waker lock and is storing a waker.
    Storing,
    // A finish has started. The finish holds the waker lock and is loading a waker.
    Loading,
    // A finish has started.
    Finished,
    // The future has been cancelled.
    Cancelled,
}

/// An AtomicWaker is a core primitive for implementing a future that can be completed by a provider
/// on another thread. The future polls the AtomicWaker until completed or cancels when dropped.
/// The provider calls finish when the future should return. The provider may also check the
/// cancellation status with is_cancelled.
///
/// There is an implicit refcount in AtomicWaker: one for the future and one for the provider. Each
/// return result may specify a refcount behavior:
/// * leaves ownership: this operation did not modify the refcount, so the object may be used again.
/// * sends ownership: this operation reduced the refcount to 1, so the object may not be used again in the current context.
/// * receives ownership: this operation dropped the refcount to 0, so the object is owned by the current context.
/// This property can be used to perform unsynchronized operations on the enclosing data structure
/// such as releasing memory.
pub struct AtomicWaker {
    state: Atomic<Flag>,
    waker: UnsafeCell<Option<Waker>>,
}

impl Packable for Flag {
    unsafe fn encode(val: Self) -> usize {
        mem::transmute::<_, u8>(val) as usize
    }

    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val as u8)
    }
}

pub enum PollResult {
    /// The future is not finished. Try again later.
    /// Leaves ownership
    Pending,
    /// The future is finished.
    /// Sends ownership
    Finished,
    /// The future is finished.
    /// Receives ownership
    FinishedFree,
}

pub enum FinishResult {
    /// The future was cancelled before it could be finished.
    /// Receives ownership
    Cancelled,
    /// The future was successfully finished.
    /// Sends ownership
    Finished,
    /// The future was successfully finished.
    /// Receives ownership
    FinishedFree,
}

pub enum CancelResult {
    /// The future was successfully cancelled.
    /// Sends ownership
    Cancelled,
    /// The future was finished before it could be cancelled.
    /// Receives ownership
    FinishedFree,
}

impl AtomicWaker {
    pub fn new(waker: Waker) -> Self {
        AtomicWaker {
            state: Atomic::new(Sleeping),
            waker: UnsafeCell::new(Some(waker)),
        }
    }

    // Poll until the future is finished.
    #[must_use]
    pub fn poll(&self, waker: &Waker) -> PollResult {
        unsafe {
            self.state.transact(|mut state| {
                Ok(match *state {
                    Sleeping => {
                        *state = Storing;
                        state.commit()?;
                        (*self.waker.get()) = Some(waker.clone());
                        self.state.transact(|mut state| {
                            Ok(match *state {
                                Sleeping => unreachable!(),
                                Storing => {
                                    *state = Sleeping;
                                    state.commit()?;
                                    PollResult::Pending
                                }
                                Loading => unreachable!(),
                                Finished => {
                                    PollResult::FinishedFree
                                }
                                Cancelled => panic!("Poll after cancel"),
                            })
                        })
                    }
                    Storing => panic!("Concurrent poll"),
                    Loading => {
                        *state = Finished;
                        state.commit()?;
                        PollResult::Finished
                    }
                    Finished => PollResult::FinishedFree,
                    Cancelled => panic!("Poll after cancel"),
                })
            })
        }
    }

    // Signal that the future is finished.
    pub fn finish(&self) -> FinishResult {
        unsafe {
            self.state.transact(|mut state| {
                Ok(match *state {
                    Sleeping => {
                        *state = Loading;
                        state.commit()?;
                        (*self.waker.get()).take().unwrap().wake();
                        self.state.transact(|mut state| {
                            Ok(match *state {
                                Sleeping => unreachable!(),
                                Storing => unreachable!(),
                                Loading => {
                                    *state = Finished;
                                    state.commit()?;
                                    FinishResult::Finished
                                }
                                Finished => FinishResult::FinishedFree,
                                Cancelled => FinishResult::Cancelled
                            })
                        })
                    }
                    Storing => {
                        *state = Finished;
                        state.commit()?;
                        FinishResult::Finished
                    }
                    Loading => panic!("Concurrent finish"),
                    Finished => panic!("Finished twice"),
                    Cancelled => FinishResult::Cancelled,
                })
            })
        }
    }

    // Signal that the future has been dropped.
    pub fn cancel(&self) -> CancelResult {
        self.state.transact(|mut state| {
            Ok(match *state {
                Sleeping => {
                    *state = Cancelled;
                    state.commit()?;
                    CancelResult::Cancelled
                }
                Storing => panic!("Cancel while polling"),
                Loading => {
                    *state = Cancelled;
                    state.commit()?;
                    CancelResult::Cancelled
                }
                Finished => CancelResult::FinishedFree,
                Cancelled => panic!("Double cancel"),
            })
        })
    }

    // Check if the future has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.state.load(Acquire) == Cancelled
    }
}
