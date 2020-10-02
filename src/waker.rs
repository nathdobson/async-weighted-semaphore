use std::cell::UnsafeCell;
use std::task::{Waker, Poll, Context};
use std::sync::atomic::Ordering::{Acquire, SeqCst};
use crate::waker::Flag::{Finished, Cancelled, Cancelling};
use crate::waker::Flag::ReadyEmpty;
use crate::waker::Flag::StoringEmpty;
use crate::waker::Flag::LoadingEmpty;
use crate::waker::Flag::LoadingStoring;
use crate::waker::Flag::LoadingReady;
use crate::atomic::{Atomic, Packable};
use std::{mem, thread, fmt};
use std::ops::Deref;
use std::thread::Thread;
use std::fmt::{Debug, Formatter};

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
#[repr(align(8))]
enum Flag {
    ReadyEmpty { front: u8 },
    StoringEmpty { front: u8 },
    LoadingEmpty { front: u8 },
    LoadingStoring { front: u8 },
    LoadingReady { front: u8 },
    Finished { poisoned: bool },
    Cancelling,
    Cancelled,
}

pub struct AtomicWaker {
    state: Atomic<Flag>,
    wakers: [UnsafeCell<Option<Waker>>; 2],
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
        mem::transmute::<_, u64>(val) as usize
    }

    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val as u64)
    }
}

impl AtomicWaker {
    pub unsafe fn new() -> Self {
        AtomicWaker {
            state: Atomic::new(ReadyEmpty { front: 0 }),
            thread: UnsafeCell::new(None),
            wakers: [UnsafeCell::new(None), UnsafeCell::new(None)],
        }
    }

    // self.state.transact(|mut state| {
    //     Ok(match *state {
    //         ReadyEmpty { front } => {},
    //         StoringEmpty { front } => {},
    //         LoadingEmpty { front } => {},
    //         LoadingStoring { front } => {},
    //         LoadingReady { front } => {},
    //         Finished { poisoned } => {},
    //         Cancelling => {},
    //         Cancelled => {},
    //     })
    // })

    // Poll until the future is finished.
    #[must_use]
    pub unsafe fn poll(&self, context: &mut Context) -> Poll<bool> {
        if let Poll::Ready(poisoned) = self.state.transact(|mut state| {
            Ok(match *state {
                ReadyEmpty { front } => {
                    *state = StoringEmpty { front };
                    state.commit()?;
                    *self.wakers[front as usize].get() = Some(context.waker().clone());
                    Poll::Pending
                }
                LoadingEmpty { front } | LoadingReady { front } => {
                    *state = LoadingStoring { front };
                    state.commit()?;
                    *self.wakers[1 - front as usize].get() = Some(context.waker().clone());
                    Poll::Pending
                }
                Finished { poisoned } => Poll::Ready(poisoned),
                _ => unreachable!("{:?}", *state)
            })
        }) {
            return Poll::Ready(poisoned);
        }
        self.state.transact(|mut state| {
            Ok(match *state {
                StoringEmpty { front } => {
                    *state = ReadyEmpty { front };
                    state.commit()?;
                    Poll::Pending
                }
                LoadingStoring { front } => {
                    *state = LoadingReady { front };
                    state.commit()?;
                    Poll::Pending
                }
                Finished { poisoned } => {
                    Poll::Ready(poisoned)
                }
                _ => unreachable!("{:?}", *state)
            })
        })
    }

    // Signal that the future is finished.
    pub unsafe fn finish(this: *const Self, poisoned: bool) -> FinishResult {
        (*this).state.transact(|mut state| {
            Ok(match *state {
                ReadyEmpty { front } => {
                    *state = LoadingEmpty { front };
                    let state = state.commit()?;
                    (*(*this).wakers[front as usize].get()).take().unwrap().wake();
                    Err(state)?
                }
                StoringEmpty { .. } | LoadingEmpty { .. } | LoadingStoring { .. } => {
                    *state = Finished { poisoned };
                    state.commit()?;
                    FinishResult::Finished { poisoned }
                }
                LoadingReady { front } => {
                    *state = LoadingEmpty { front: 1 - front };
                    let state = state.commit()?;
                    (*(*this).wakers[1 - front as usize].get()).take().unwrap().wake();
                    Err(state)?
                }
                Cancelling => {
                    FinishResult::Cancelling
                }
                _ => unreachable!()
            })
        })
    }

    // Signal that the future has been dropped.
    pub unsafe fn start_cancel(&self) -> CancelResult {
        *self.thread.get() = Some(thread::current());
        self.state.transact(|mut state| {
            Ok(match *state {
                ReadyEmpty { .. }
                | LoadingEmpty { .. }
                | LoadingReady { .. } => {
                    *state = Cancelling;
                    state.commit()?;
                    CancelResult::Cancelling
                }
                Finished { poisoned } => {
                    CancelResult::Finished { poisoned }
                }
                _ => unreachable!("{:?}", *state)
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

    pub unsafe fn accept_cancel(this: *const Self) {
        (*this).state.transact(|mut state| {
            Ok(match *state {
                Cancelling => {
                    *state = Cancelled;
                    let thread = (*(*this).thread.get()).take().unwrap();
                    state.commit()?;
                    thread.unpark();
                }
                _ => unreachable!("{:?}", *state)
            })
        })
    }
}

impl Debug for AtomicWaker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AW")
            .field("s", &self.state.load(SeqCst))
            .field("w0", &(unsafe { mem::transmute::<_, &(*mut (), *mut ())>(&*self.wakers[0].get()) }.0))
            .field("w1", &(unsafe { mem::transmute::<_, &(*mut (), *mut ())>(&*self.wakers[1].get()) }.0))
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
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
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

                match tester.waiter.start_cancel() {
                    CancelResult::Cancelling => {
                        AtomicWaker::accept_cancel(&tester.as_mut().waiter);
                        tester.waiter.wait_cancel();
                    }
                    CancelResult::Finished { .. } => panic!(),
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
                match tester.waiter.start_cancel() {
                    CancelResult::Cancelling => {
                        AtomicWaker::accept_cancel(&tester.waiter);
                        tester.waiter.wait_cancel();
                    }
                    CancelResult::Finished { .. } => panic!(),
                }
            }
        });
    }

    fn test_race(cancel: bool) {
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
                    let waiter = recv.recv().unwrap();
                    let result =
                        match AtomicWaker::finish(waiter, i % 2 == 0) {
                            FinishResult::Cancelling => {
                                AtomicWaker::accept_cancel(waiter);
                                FinishResult::Cancelling
                            }
                            FinishResult::Finished { poisoned } =>
                                FinishResult::Finished { poisoned }
                        };
                    results.push(result);
                }
                results
            }));
            let r1 = h1.join().unwrap();
            let r2 = h2.join().unwrap();
            for (send, recv) in r1.into_iter().zip(r2.into_iter()) {
                match (cancel, send, recv) {
                    (_, Some(o), FinishResult::Finished { poisoned: i }) if i == o => {}
                    (true, None, FinishResult::Cancelling) => {}
                    _ => panic!("Unexpected outcome {:?}", (cancel, send, recv))
                }
            }
        }
    }

    #[test]
    fn test_poll_race() {
        test_race(false);
    }

    #[test]
    fn test_cancel_race() {
        test_race(true);
    }
}
