use std::cell::UnsafeCell;
use std::task::{Waker, Poll, Context};
use std::sync::atomic::Ordering::{Acquire, SeqCst};
use crate::waker::Flag::{Sleeping, Storing, Finished, Cancelled, Loading, Cancelling};
use crate::atomic::{Atomic, Packable};
use std::{mem, thread, fmt};
use std::ops::Deref;
use std::thread::Thread;
use std::fmt::{Debug, Formatter};

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
enum Flag {
    Sleeping,
    Storing,
    Loading { poisoned: bool },
    Finished { poisoned: bool },
    Cancelling,
    Cancelled,
}

pub struct AtomicWaker {
    state: Atomic<Flag>,
    waker: UnsafeCell<Option<Waker>>,
    thread: UnsafeCell<Option<Thread>>,
}


#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
pub enum FinishResult {
    Cancelling,
    Finished { poisoned: bool },
}

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
pub enum CancelResult {
    Cancelling,
    Finished { poisoned: bool },
}

unsafe impl Send for AtomicWaker {}

unsafe impl Sync for AtomicWaker {}

impl Packable for Flag {
    unsafe fn encode(val: Self) -> usize {
        mem::transmute::<_, u16>(val) as usize
    }

    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val as u16)
    }
}

impl AtomicWaker {
    pub unsafe fn new() -> Self {
        AtomicWaker {
            state: Atomic::new(Sleeping),
            waker: UnsafeCell::new(None),
            thread: UnsafeCell::new(None),
        }
    }

    // Poll until the future is finished.
    #[must_use]
    pub unsafe fn poll(&self, context: &mut Context) -> Poll<bool> {
        self.state.transact(|mut state| {
            Ok(match *state {
                Sleeping => {
                    *state = Storing;
                    state.commit()?;
                    (*self.waker.get()) = Some(context.waker().clone());
                    self.state.transact(|mut state| {
                        Ok(match *state {
                            Sleeping => unreachable!(),
                            Storing => {
                                *state = Sleeping;
                                state.commit()?;
                                Poll::Pending
                            }
                            Loading { poisoned } => unreachable!(),
                            Finished { poisoned } => Poll::Ready(poisoned),
                            Cancelled => panic!("Poll after cancel"),
                            Cancelling => panic!("Poll after cancel"),
                        })
                    })
                }
                Storing => panic!("Concurrent poll"),
                Loading { poisoned } => {
                    *state = Finished { poisoned };
                    state.commit()?;
                    Poll::Ready(poisoned)
                }
                Finished { poisoned } => Poll::Ready(poisoned),
                Cancelled => panic!("Poll after cancel"),
                Cancelling => panic!("Poll after cancel"),
            })
        })
    }

    // Signal that the future is finished.
    pub unsafe fn finish(this: *const Self, poisoned: bool) -> FinishResult {
        (*this).state.transact(|mut state| {
            //println!("Finishing {:?} {:?}", self as *const Self, *state);
            Ok(match *state {
                Sleeping => {
                    *state = Loading { poisoned };
                    state.commit()?;
                    (*(*this).waker.get()).take().unwrap().wake();
                    (*this).state.transact(|mut state| {
                        Ok(match *state {
                            Sleeping => unreachable!(),
                            Storing => unreachable!(),
                            Loading { poisoned } => {
                                *state = Finished { poisoned };
                                state.commit()?;
                                FinishResult::Finished { poisoned }
                            }
                            Finished { poisoned } =>
                                FinishResult::Finished { poisoned },

                            Cancelling => {
                                FinishResult::Cancelling
                            }
                            Cancelled => panic!("Finish twice"),
                        })
                    })
                }
                Storing => {
                    *state = Finished { poisoned };
                    state.commit()?;
                    FinishResult::Finished { poisoned }
                }
                Loading { poisoned } => panic!("Concurrent finish"),
                Finished { poisoned } => panic!("Finished twice"),
                Cancelling => {
                    FinishResult::Cancelling
                }
                Cancelled => panic!("Finish twice"),
            })
        })
    }

    // Signal that the future has been dropped.
    pub unsafe fn start_cancel(&self) -> CancelResult {
        self.state.transact(|mut state| {
            Ok(match *state {
                Sleeping | Loading { .. } => {
                    *self.thread.get() = Some(thread::current());
                    *state = Cancelling;
                    state.commit()?;
                    CancelResult::Cancelling
                }
                Storing => panic!("Cancel while polling"),
                Finished { poisoned } => {
                    CancelResult::Finished { poisoned }
                }
                Cancelling => panic!("Double cancel"),
                Cancelled => panic!("Double cancel"),
            })
        })
    }

    pub unsafe fn wait_cancel(&self) {
        loop {
            match self.state.load(Acquire) {
                Cancelling => thread::park(),
                Cancelled => break,
                _ => panic!(),
            }
        }
    }

    // Check if the future has been cancelled.
    pub unsafe fn check_cancelled(this: *const Self) -> bool {
        (*this).state.transact(|mut state| {
            Ok(match *state {
                Sleeping => false,
                Storing => false,
                Loading { poisoned } => panic!("Checking during finish"),
                Finished { poisoned } => panic!("Checking after finish"),
                Cancelling => {
                    *state = Cancelled;
                    state.commit()?;
                    (*(*this).thread.get()).as_ref().unwrap().unpark();
                    true
                }
                Cancelled => panic!("Double finish")
            })
        })
    }
}

impl Debug for AtomicWaker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AW")
            .field("s", &self.state.load(SeqCst))
            .field("w", &(unsafe { mem::transmute::<_, &(*mut (), *mut ())>(&*self.waker.get()) }.0))
            .field("t", unsafe { &*self.thread.get() })
            .finish()
    }
}

#[cfg(test)]
mod test {
    use crate::waker::{FinishResult, CancelResult, AtomicWaker};
    use std::future::Future;
    use futures::task::{Context, Poll};
    use std::pin::Pin;
    use std::ptr::null;
    use futures::pin_mut;
    use futures::poll;
    use futures::join;
    use std::cell::UnsafeCell;
    use std::sync::mpsc::channel;
    use std::sync::{Barrier, Arc};
    use async_std::task::spawn;
    use futures::executor::block_on;
    use std::thread;
    use futures_test::std_reexport::sync::mpsc::sync_channel;

    struct Tester {
        waiter: AtomicWaker,
    }

    unsafe impl Send for Tester {}

    unsafe impl Sync for Tester {}

    impl Unpin for Tester {}

    impl Future for Tester {
        type Output = bool;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
            unsafe {
                self.waiter.poll(cx)
            }
        }
    }

    #[test]
    fn test_finish() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: AtomicWaker::new() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!AtomicWaker::check_cancelled(&tester.waiter as *const AtomicWaker));
                assert_eq!(FinishResult::Finished { poisoned: false },
                           AtomicWaker::finish(&tester.waiter as *const AtomicWaker, false));
                assert_eq!(Poll::Ready(false), poll!(&mut tester));
            }
        });
    }

    #[test]
    fn test_cancel() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: AtomicWaker::new() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!AtomicWaker::check_cancelled(&tester.as_mut().waiter));

                match tester.waiter.start_cancel() {
                    CancelResult::Cancelling => {
                        assert!(AtomicWaker::check_cancelled(&tester.as_mut().waiter));
                        tester.waiter.wait_cancel();
                    }
                    CancelResult::Finished { poisoned } => panic!(),
                }
            }
        });
    }

    #[test]
    fn test_finish_cancel() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: AtomicWaker::new() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!AtomicWaker::check_cancelled(&tester.as_mut().waiter));
                assert_eq!(FinishResult::Finished { poisoned: true },
                           AtomicWaker::finish(&tester.waiter, true));
                match tester.waiter.start_cancel() {
                    CancelResult::Cancelling => panic!(),
                    CancelResult::Finished { poisoned } => { assert!(poisoned) }
                }
            }
        });
    }


    #[test]
    fn test_cancel_finish() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: AtomicWaker::new() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!AtomicWaker::check_cancelled(&tester.as_mut().waiter));
                match tester.waiter.start_cancel() {
                    CancelResult::Cancelling => {
                        assert!(AtomicWaker::check_cancelled(&tester.waiter));
                        tester.waiter.wait_cancel();
                    }
                    CancelResult::Finished { poisoned } => panic!(),
                }
            }
        });
    }

    fn test_race(finish: bool, cancel: bool) {
        unsafe {
            let iters = 10000;
            let mut testers = (0..iters).map(|_| Tester { waiter: AtomicWaker::new() }).collect::<Vec<_>>();
            let (send, recv) = sync_channel(0);
            let h1 = thread::spawn(move || block_on(async {
                let mut results = vec![];
                for i in 0..iters {
                    let mut tester = &mut testers[i];
                    assert_eq!(Poll::Pending, poll!(&mut tester));
                    send.send(&*(&tester.waiter as *const AtomicWaker)).unwrap();
                    let result = if cancel {
                        match tester.waiter.start_cancel() {
                            CancelResult::Cancelling => {
                                tester.waiter.wait_cancel();
                                None
                            }
                            CancelResult::Finished { poisoned } => Some(poisoned),
                        }
                    } else {
                        Some(tester.await)
                    };
                    results.push(result);
                }
                results
            }));
            let h2 = thread::spawn(move || block_on(async {
                let mut results = vec![];
                for i in 0..iters {
                    let mut waiter = recv.recv().unwrap();
                    let result = if finish {
                        match AtomicWaker::finish(waiter, i % 2 == 0) {
                            FinishResult::Cancelling => {
                                AtomicWaker::check_cancelled(waiter);
                                FinishResult::Cancelling
                            }
                            FinishResult::Finished { poisoned } => FinishResult::Finished { poisoned }
                        }
                    } else {
                        while !AtomicWaker::check_cancelled(waiter) {}
                        FinishResult::Cancelling
                    };
                    results.push(result);
                }
                results
            }));
            let r1 = h1.join().unwrap();
            let r2 = h2.join().unwrap();
            for (i, (send, recv)) in r1.into_iter().zip(r2.into_iter()).enumerate() {
                match (finish, cancel, send, recv) {
                    (true, _, Some(o), FinishResult::Finished { poisoned: i }) if i == o => {}
                    (_, true, None, FinishResult::Cancelling) => {}
                    _ => panic!("Unexpected outcome {:?}", (finish, cancel, send, recv))
                }
            }
        }
    }

    #[test]
    fn test_finish_poll_race() {
        test_race(true, false);
    }

    #[test]
    fn test_poll_cancel_race() {
        test_race(false, true);
    }

    #[test]
    fn test_finish_cancel_race() {
        test_race(true, true);
    }
}
