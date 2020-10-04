use std::cell::UnsafeCell;
use std::task::{Waker, Poll, Context, RawWakerVTable};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Acquire, SeqCst, Relaxed};
use crate::waker::State::{Cancelled, Cancelling};
use crate::atomic::{Atomic, Packable};
use std::{mem, thread, fmt};
use std::ops::Deref;
use std::thread::Thread;
use std::fmt::{Debug, Formatter};
use std::ptr::null;
use crate::waker::State::{Pending, Storing, Finished, Loading};

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
enum State {
    // There is a valid waker
    Pending,
    // A poll is in progress, storing a new waker.
    Storing,
    // A finish is in progress, loading the waker.
    Loading,
    // A finish has succeeded, and will call wake if necessary.
    Finished { poisoned: bool },
    // start_cancel has been called.
    Cancelling,
    // accept_cancel has been called.
    Cancelled,
}

// A primitive for synchronizing the polling or cancellation of a Future with another thread
// that marks the future as finished.
pub struct AtomicWaker {
    vtable: Atomic<*const RawWakerVTable>,
    // Split the waker among two atomics. These only need to be atomic to prevent data races.
    data: Atomic<*const ()>,
    state: Atomic<State>,
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

impl Packable for State {
    unsafe fn encode(val: Self) -> usize {
        mem::transmute::<_, u8>(val) as usize
    }

    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val as u8)
    }
}

impl AtomicWaker {
    pub unsafe fn new() -> Self {
        AtomicWaker {
            state: Atomic::new(Pending),
            vtable: Atomic::new(null()),
            data: Atomic::new(null()),
            thread: UnsafeCell::new(None),
        }
    }

    // Poll until the future is finished.
    #[must_use]
    pub unsafe fn poll(&self, context: &mut Context) -> Poll<bool> {
        let mut waker = Some(context.waker().clone());
        self.state.transact(|mut state| {
            Ok(match *state {
                Pending | Loading => {
                    *state = Storing;
                    state.commit()?;
                    let (data, vtable) =
                        mem::transmute_copy::<_, (*const (), *const RawWakerVTable)>(waker.as_ref().unwrap());
                    let old_data = self.data.load(Relaxed);
                    let old_vtable = self.vtable.load(Relaxed);
                    self.data.store(data, Relaxed);
                    self.vtable.store(vtable, Relaxed);
                    if old_vtable != null() {
                        mem::drop(mem::transmute::<_, Waker>((old_data, old_vtable)));
                    }
                    self.state.transact(|mut state| {
                        Ok(match *state {
                            Storing => {
                                *state = Pending;
                                state.commit()?;
                                mem::forget(waker.take());
                                Poll::Pending
                            }
                            Finished { poisoned } => {
                                Poll::Ready(poisoned)
                            }
                            _ => unreachable!()
                        })
                    })
                }
                Finished { poisoned } => Poll::Ready(poisoned),
                _ => unreachable!()
            })
        })
    }

    // Signal that the future is finished.
    pub unsafe fn finish(this: *const Self, poisoned: bool) -> FinishResult {
        (*this).state.transact(|mut state| {
            Ok(match *state {
                Pending => {
                    *state = Loading;
                    let new = state.commit()?;
                    Err(new)?
                }
                Loading => {
                    let data = (*this).data.load(Relaxed);
                    let vtable = (*this).vtable.load(Relaxed);
                    *state = Finished { poisoned };
                    state.commit()?;
                    let waker = mem::transmute::<_, Waker>((data, vtable));
                    waker.wake();
                    FinishResult::Finished { poisoned }
                }
                Storing => {
                    *state = Finished { poisoned };
                    state.commit()?;
                    FinishResult::Finished { poisoned }
                }
                Cancelling => FinishResult::Cancelling,
                _ => unreachable!()
            })
        })
    }

    // Signal that the future has been dropped.
    pub unsafe fn start_cancel(&self) -> CancelResult {
        *self.thread.get() = Some(thread::current());
        self.state.transact(|mut state| {
            Ok(match *state {
                Pending | Loading => {
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
                    if let Err(e) = state.commit() {
                        *(*this).thread.get() = Some(thread);
                        return Err(e);
                    }
                    thread.unpark();
                }
                _ => unreachable!("{:?}", *state)
            })
        })
    }
}

impl Drop for AtomicWaker {
    fn drop(&mut self) {
        match self.state.load(Relaxed) {
            Pending | Cancelled => unsafe {
                let data = self.data.load(Relaxed);
                let vtable = self.vtable.load(Relaxed);
                if vtable != null() {
                    mem::drop(mem::transmute::<_, Waker>((data, vtable)));
                }
            }
            Finished { .. } => {}
            _ => unreachable!()
        }
    }
}

impl Debug for AtomicWaker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AW")
            .field("s", &self.state.load(SeqCst))
            .field("d", &self.data.load(SeqCst))
            .field("t", &self.vtable.load(SeqCst))
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
