use std::cell::UnsafeCell;
use std::task::{Waker};
use std::sync::atomic::Ordering::{Acquire};
use crate::waker::Flag::{Sleeping, Storing, Finished, Cancelled, Loading};
use crate::atomic::{Atomic, Packable};

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
enum Flag {
    Sleeping,
    Storing,
    Loading,
    Finished,
    Cancelled,
}

pub struct AtomicWaker {
    state: Atomic<Flag>,
    waker: UnsafeCell<Option<Waker>>,
}

impl Packable for Flag { type Raw = u8; }

pub enum PollResult {
    Pending,
    Finished,
    FinishedFree,
}

pub enum FinishResult {
    Cancelled,
    Finished,
    FinishedFree,
}

pub enum CancelResult {
    Cancelled,
    FinishedFree,
}

impl AtomicWaker {
    pub fn new(waker: Waker) -> Self {
        AtomicWaker {
            state: Atomic::new(Sleeping),
            waker: UnsafeCell::new(Some(waker)),
        }
    }
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

    pub fn is_cancelled(&self) -> bool {
        self.state.load(Acquire) == Cancelled
    }
}
