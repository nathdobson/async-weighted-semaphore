use std::cell::UnsafeCell;
use std::task::{Waker, Poll, Context, RawWakerVTable};

use std::sync::atomic::Ordering::{Acquire, SeqCst, Relaxed};
use crate::waker::State::{Cancelled, Cancelling};
use crate::atomic::{Atomic, Packable};
use std::{mem, thread, fmt};

use std::thread::{Thread, panicking};
use std::fmt::{Debug, Formatter};
use std::ptr::null;
use crate::waker::State::{Pending, Storing, Finished};

#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
enum State {
    Pending { seq: usize },
    // A poll is in progress, storing a new waker.
    Storing { seq: usize },
    // A finish has succeeded, and will call wake if necessary.
    Finished { poisoned: bool },
    // start_cancel has been called.
    Cancelling,
    // accept_cancel has been called.
    Cancelled,
}

// A primitive for synchronizing between two threads:
// Future: a thread that polls the a future and may decide to cancel (drop) it.
// Producer: one that marks the future as finished or accepts cancellation.
pub struct AtomicWaker {
    vtable: Atomic<*const RawWakerVTable>,
    // Split the waker among two atomics. These only need to be atomic to prevent data races.
    data: Atomic<*const ()>,
    state: Atomic<State>,
    // The thread waiting for acceptance of cancellation
    thread: UnsafeCell<Option<Thread>>,
}

// The result of an attempt to finish or cancel.
#[derive(Copy, Clone, Eq, PartialOrd, PartialEq, Ord, Debug)]
pub enum WakerResult {
    // The future has been cancelled. The producer should accept_cancel.
    // The future thread should wait_cancel.
    Cancelling,
    // The future completed.
    Finished { poisoned: bool },
}

unsafe impl Send for AtomicWaker {}

unsafe impl Sync for AtomicWaker {}

impl Packable for State {
    unsafe fn encode(val: Self) -> usize {
        match val {
            Pending { seq } => seq << 3 | 0,
            Storing { seq } => seq << 3 | 1,
            Finished { poisoned: false } => 2,
            Finished { poisoned: true } => 3,
            Cancelling => 4,
            Cancelled => 5,
        }
    }

    unsafe fn decode(val: usize) -> Self {
        match val & 7 {
            0 => Pending { seq: val >> 3 },
            1 => Storing { seq: val >> 3 },
            2 => Finished { poisoned: false },
            3 => Finished { poisoned: true },
            4 => Cancelling,
            5 => Cancelled,
            _ => unreachable!()
        }
    }
}

impl AtomicWaker {
    pub unsafe fn new() -> Self {
        AtomicWaker {
            state: Atomic::new(Pending { seq: 0 }),
            vtable: Atomic::new(null()),
            data: Atomic::new(null()),
            thread: UnsafeCell::new(None),
        }
    }

    // Store a new waker and return the poisoned bit if finished.
    #[must_use]
    pub unsafe fn poll(&self, context: &mut Context) -> Poll<bool> {
        let mut waker = Some(context.waker().clone());
        let mut current = self.state.load(Acquire);
        loop {
            match current {
                Pending { seq } => {
                    if self.state.cmpxchg_weak_acqrel(&mut current, Storing { seq }) {
                        current = Storing { seq };
                        break;
                    }
                }
                Finished { poisoned } => return Poll::Ready(poisoned),
                _ => unreachable!()
            };
        }
        let (data, vtable) =
            mem::transmute_copy::<_, (*const (), *const RawWakerVTable)>(waker.as_ref().unwrap());
        let old_data = self.data.load(Relaxed);
        let old_vtable = self.vtable.load(Relaxed);
        self.data.store(data, Relaxed);
        self.vtable.store(vtable, Relaxed);
        if old_vtable != null() {
            mem::drop(mem::transmute::<_, Waker>((old_data, old_vtable)));
        }
        loop {
            match current {
                Storing { seq } => {
                    if self.state.cmpxchg_weak_acqrel(&mut current, Pending { seq: seq + 1 }) {
                        mem::forget(waker.take());
                        return Poll::Pending;
                    }
                }
                Finished { poisoned } => {
                    // Ownership is exclusive at this point, so no memory barrier is needed.
                    return Poll::Ready(poisoned);
                }
                _ => unreachable!("{:?}", current)
            }
        }
    }

    // Signal that the future is finished.
    pub unsafe fn finish(this: *const Self, poisoned: bool) -> WakerResult {
        let mut current = (*this).state.load(Acquire);
        loop {
            match current {
                Pending { .. } => {
                    let data = (*this).data.load(Relaxed);
                    let vtable = (*this).vtable.load(Relaxed);
                    if (*this).state.cmpxchg_weak_acqrel(&mut current, Finished { poisoned }) {
                        let waker = mem::transmute::<_, Waker>((data, vtable));
                        waker.wake();
                        return WakerResult::Finished { poisoned };
                    }
                }
                Storing { .. } => {
                    if (*this).state.cmpxchg_weak_acqrel(&mut current, Finished { poisoned }) {
                        return WakerResult::Finished { poisoned };
                    }
                }
                Cancelling => return WakerResult::Cancelling,
                _ => unreachable!()
            }
        }
    }

    pub unsafe fn start_cancel(&self) -> WakerResult {
        *self.thread.get() = Some(thread::current());
        let mut current = self.state.load(Acquire);
        loop {
            match current {
                Pending { .. } => {
                    if self.state.cmpxchg_weak_acqrel(&mut current, Cancelling) {
                        return WakerResult::Cancelling;
                    }
                }
                Finished { poisoned } => {
                    return WakerResult::Finished { poisoned };
                }
                _ => unreachable!("{:?}", current)
            }
        }
    }

    // Wait for the producer thread to accept cancellation
    pub unsafe fn wait_cancel(&self) {
        loop {
            match self.state.load(Acquire) {
                Cancelling => thread::park(),
                Cancelled => break,
                _ => panic!(),
            }
        }
    }

    // Accept cancellation on the producer thread, notifying the future thread.
    pub unsafe fn accept_cancel(this: *const Self) {
        let mut current = (*this).state.load(Acquire);
        loop {
            match current {
                Cancelling => {
                    let thread = (*(*this).thread.get()).take().unwrap();
                    if (*this).state.cmpxchg_weak_acqrel(&mut current, Cancelled) {
                        thread.unpark();
                        break;
                    } else {
                        *(*this).thread.get() = Some(thread);
                        continue;
                    }
                }
                _ => unreachable!("{:?}", current)
            }
        }
    }
}

impl Drop for AtomicWaker {
    fn drop(&mut self) {
        match self.state.load(Relaxed) {
            Pending { .. } | Cancelled => unsafe {
                let data = self.data.load(Relaxed);
                let vtable = self.vtable.load(Relaxed);
                if vtable != null() {
                    mem::drop(mem::transmute::<_, Waker>((data, vtable)));
                }
            }
            Finished { .. } => {}
            _ => if !panicking() { unreachable!() }
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
    use crate::waker::{WakerResult, AtomicWaker};
    use std::future::Future;
    use futures::task::{Context, Poll};
    use std::pin::Pin;

    use futures::pin_mut;
    use futures::poll;


    use futures::executor::block_on;
    use std::thread;
    use std::sync::mpsc::sync_channel;

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
                assert_eq!(WakerResult::Finished { poisoned: false },
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
                    WakerResult::Cancelling => {
                        AtomicWaker::accept_cancel(&tester.as_mut().waiter);
                        tester.waiter.wait_cancel();
                    }
                    WakerResult::Finished { .. } => panic!(),
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
                assert_eq!(WakerResult::Finished { poisoned: true },
                           AtomicWaker::finish(&tester.waiter, true));
                match tester.waiter.start_cancel() {
                    WakerResult::Cancelling => panic!(),
                    WakerResult::Finished { poisoned } => { assert!(poisoned) }
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
                    WakerResult::Cancelling => {
                        AtomicWaker::accept_cancel(&tester.waiter);
                        tester.waiter.wait_cancel();
                    }
                    WakerResult::Finished { .. } => panic!(),
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
                            WakerResult::Cancelling => {
                                tester.waiter.wait_cancel();
                                None
                            }
                            WakerResult::Finished { poisoned } => Some(poisoned),
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
                            WakerResult::Cancelling => {
                                AtomicWaker::accept_cancel(waiter);
                                WakerResult::Cancelling
                            }
                            WakerResult::Finished { poisoned } =>
                                WakerResult::Finished { poisoned }
                        };
                    results.push(result);
                }
                results
            }));
            let r1 = h1.join().unwrap();
            let r2 = h2.join().unwrap();
            for (send, recv) in r1.into_iter().zip(r2.into_iter()) {
                match (cancel, send, recv) {
                    (_, Some(o), WakerResult::Finished { poisoned: i }) if i == o => {}
                    (true, None, WakerResult::Cancelling) => {}
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
