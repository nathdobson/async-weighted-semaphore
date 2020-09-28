use std::cell::UnsafeCell;
use std::task::{Waker};
use std::sync::atomic::Ordering::{Acquire};
use crate::waker::Flag::{Sleeping, Storing, Finished, Cancelled, Loading};
use crate::atomic::{Atomic, Packable};
use std::mem;

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
pub enum Outcome {
    Acquire,
    Poison,
}

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
enum Flag {
    // There is a valid waker. The waker lock is not held. No operations are in progress.
    Sleeping,
    // A spurious poll has occurred. The poll holds the waker lock and is storing a waker.
    Storing,
    // A finish has started. The finish holds the waker lock and is loading a waker.
    Loading(Outcome),
    // A finish has started.
    Finished(Outcome),
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
        mem::transmute::<_, u16>(val) as usize
    }

    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val as u16)
    }
}

pub enum PollResult {
    /// The future is not finished. Try again later.
    Pending,
    /// The future is finished.
    Finished { free: bool, outcome: Outcome },
}

pub enum FinishResult {
    /// The future was cancelled before it could be finished.
    CancelledFree,
    /// The future was successfully finished.
    Finished { free: bool, outcome: Outcome },
}

pub enum CancelResult {
    /// The future was successfully cancelled.
    Cancelled,
    /// The future was finished before it could be cancelled.
    FinishedFree { outcome: Outcome },
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
                                Loading(_) => unreachable!(),
                                Finished(outcome) => {
                                    PollResult::Finished { free: true, outcome }
                                }
                                Cancelled => panic!("Poll after cancel"),
                            })
                        })
                    }
                    Storing => panic!("Concurrent poll"),
                    Loading(outcome) => {
                        *state = Finished(outcome);
                        state.commit()?;
                        PollResult::Finished { free: false, outcome }
                    }
                    Finished(outcome) => PollResult::Finished { free: true, outcome },
                    Cancelled => panic!("Poll after cancel"),
                })
            })
        }
    }

    // Signal that the future is finished.
    pub fn finish(&self, outcome: Outcome) -> FinishResult {
        unsafe {
            self.state.transact(|mut state| {
                Ok(match *state {
                    Sleeping => {
                        *state = Loading(outcome);
                        state.commit()?;
                        (*self.waker.get()).take().unwrap().wake();
                        self.state.transact(|mut state| {
                            Ok(match *state {
                                Sleeping => unreachable!(),
                                Storing => unreachable!(),
                                Loading(_) => {
                                    *state = Finished(outcome);
                                    state.commit()?;
                                    FinishResult::Finished { free: false, outcome }
                                }
                                Finished(outcome) => FinishResult::Finished { free: true, outcome },
                                Cancelled => FinishResult::CancelledFree
                            })
                        })
                    }
                    Storing => {
                        *state = Finished(outcome);
                        state.commit()?;
                        FinishResult::Finished { free: false, outcome }
                    }
                    Loading(_) => panic!("Concurrent finish"),
                    Finished(_) => panic!("Finished twice"),
                    Cancelled => FinishResult::CancelledFree,
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
                Loading(_) => {
                    *state = Cancelled;
                    state.commit()?;
                    CancelResult::Cancelled
                }
                Finished(outcome) => CancelResult::FinishedFree { outcome },
                Cancelled => panic!("Double cancel"),
            })
        })
    }

    // Check if the future has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.state.load(Acquire) == Cancelled
    }
}
