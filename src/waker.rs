use std::cell::UnsafeCell;
use std::task::{Waker, Poll, Context};
use std::sync::atomic::Ordering::{Acquire};
use crate::waker::Flag::{Sleeping, Storing, Finished, Cancelled, Loading};
use crate::atomic::{Atomic, Packable};
use std::mem;
use std::ops::Deref;

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

pub struct Waiter<T> {
    state: Atomic<Flag>,
    waker: UnsafeCell<Option<Waker>>,
    contents: UnsafeCell<Option<T>>,
}

impl Packable for Flag {
    unsafe fn encode(val: Self) -> usize {
        mem::transmute::<_, u8>(val) as usize
    }

    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val as u8)
    }
}

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
pub enum FinishResult {
    Cancelled,
    Finished,
}

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
pub enum CancelResult<T> {
    Cancelled,
    Finished(T),
}

impl<T> Waiter<T> {
    pub unsafe fn new(cx: &mut Context, contents: T) -> *const Self {
        Box::into_raw(Box::new(Waiter {
            state: Atomic::new(Sleeping),
            waker: UnsafeCell::new(Some(cx.waker().clone())),
            contents: UnsafeCell::new(Some(contents)),
        }))
    }

    unsafe fn take(&self) -> T {
        (*self.contents.get()).take().unwrap()
    }

    pub unsafe fn free(&self) -> Option<T> {
        (*Box::from_raw(self as *const Self as *mut Self).contents.get()).take()
    }

    // Poll until the future is finished.
    #[must_use]
    pub unsafe fn poll(&self, context: &mut Context) -> Poll<T> {
        self.state.transact(|mut state| {
            //println!("Polling {:?} {:?}", self as *const Self, *state);
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
                            Loading => unreachable!(),
                            Finished => Poll::Ready(self.free().unwrap()),
                            Cancelled => panic!("Poll after cancel"),
                        })
                    })
                }
                Storing => panic!("Concurrent poll"),
                Loading => {
                    let result = self.take();
                    *state = Finished;
                    if let Err(e) = state.commit() {
                        *self.contents.get() = Some(result);
                        return Err(e);
                    }
                    Poll::Ready(result)
                }
                Finished => Poll::Ready(self.free().unwrap()),
                Cancelled => panic!("Poll after cancel"),
            })
        })
    }

    // Signal that the future is finished.
    pub unsafe fn finish(&self) -> FinishResult {
        self.state.transact(|mut state| {
            //println!("Finishing {:?} {:?}", self as *const Self, *state);
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
                            Finished => {
                                self.free();
                                FinishResult::Finished
                            }
                            Cancelled => {
                                self.free();
                                FinishResult::Cancelled
                            }
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
                Cancelled => {
                    self.free();
                    FinishResult::Cancelled
                }
            })
        })
    }

    // Signal that the future has been dropped.
    pub unsafe fn cancel(&self) -> CancelResult<T> {
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
                Finished => {
                    CancelResult::Finished(self.free().unwrap())
                }
                Cancelled => panic!("Double cancel"),
            })
        })
    }

    // Check if the future has been cancelled.
    pub unsafe fn check_cancelled(&self) -> bool {
        if self.state.load(Acquire) == Cancelled {
            self.free();
            true
        } else { false }
    }
}

impl<T> Deref for Waiter<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { (*self.contents.get()).as_ref().unwrap() }
    }
}

#[cfg(test)]
mod test {
    use crate::waker::{Waiter, FinishResult, CancelResult};
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

    #[derive(Clone)]
    struct Tester {
        waiter: *const Waiter<UnsafeCell<Box<usize>>>,
    }

    unsafe impl Send for Tester {}

    unsafe impl Sync for Tester {}

    impl Unpin for Tester {}

    impl Future for Tester {
        type Output = usize;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            unsafe {
                if self.waiter == null() {
                    self.waiter = Waiter::new(cx, UnsafeCell::new(Box::new(0)));
                    Poll::Pending
                } else {
                    match (*self.waiter).poll(cx) {
                        Poll::Pending => Poll::Pending,
                        Poll::Ready(x) => Poll::Ready(**x.get())
                    }
                }
            }
        }
    }

    #[test]
    fn test_finish() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: null() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!(*tester.as_mut().waiter).check_cancelled());
                **(*tester.as_mut().waiter).get() = 1;
                assert_eq!(FinishResult::Finished, (*tester.waiter).finish());
                assert_eq!(Poll::Ready(1), poll!(&mut tester));
            }
        });
    }

    #[test]
    fn test_cancel() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: null() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!(*tester.as_mut().waiter).check_cancelled());
                match (*tester.waiter).cancel() {
                    CancelResult::Cancelled => {}
                    CancelResult::Finished(_) => panic!(),
                }
                assert!((*tester.as_mut().waiter).check_cancelled());
            }
        });
    }

    #[test]
    fn test_finish_cancel() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: null() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!(*tester.as_mut().waiter).check_cancelled());
                **(*tester.as_mut().waiter).get() = 1;
                assert_eq!(FinishResult::Finished, (*tester.waiter).finish());
                match (*tester.waiter).cancel() {
                    CancelResult::Cancelled => panic!(),
                    CancelResult::Finished(x) => { assert_eq!(**x.get(), 1) }
                }
            }
        });
    }


    #[test]
    fn test_cancel_finish() {
        block_on(async {
            unsafe {
                let tester = Tester { waiter: null() };
                pin_mut!(tester);
                assert_eq!(Poll::Pending, poll!(&mut tester));
                assert!(!(*tester.as_mut().waiter).check_cancelled());
                match (*tester.waiter).cancel() {
                    CancelResult::Cancelled => {}
                    CancelResult::Finished(_) => panic!(),
                }
                **(*tester.as_mut().waiter).get() = 1;
                assert_eq!(FinishResult::Cancelled, (*tester.waiter).finish());
            }
        });
    }

    fn test_race(finish: bool, cancel: bool) {
        unsafe {
            let iters = 10000;
            let (send, recv) = sync_channel(0);
            let h1 = thread::spawn(move || block_on(async {
                let mut results = vec![];
                for _ in 0..iters {
                    let mut tester = Tester { waiter: null() };
                    assert_eq!(Poll::Pending, poll!(&mut tester));
                    send.send(tester.clone()).unwrap();
                    let result = if cancel {
                        match (*tester.waiter).cancel() {
                            CancelResult::Cancelled => None,
                            CancelResult::Finished(data) => Some(**data.get()),
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
                    let tester = recv.recv().unwrap();
                    let result = if finish {
                        **(*tester.waiter).get() = i;
                        (*tester.waiter).finish()
                    } else {
                        while !(*tester.waiter).check_cancelled() {}
                        FinishResult::Cancelled
                    };
                    results.push(result);
                }
                results
            }));
            let r1 = h1.join().unwrap();
            let r2 = h2.join().unwrap();
            for (i, (send, recv)) in r1.into_iter().zip(r2.into_iter()).enumerate() {
                match (finish, cancel, send, recv) {
                    (true, _, Some(o), FinishResult::Finished) if i == o => {}
                    (_, true, None, FinishResult::Cancelled) => {}
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
