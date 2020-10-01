use futures::executor::{block_on, LocalPool};
use futures::task::{LocalSpawnExt, noop_waker_ref, SpawnExt};
use std::sync::{Arc, Barrier, Mutex};
use std::rc::Rc;
use std::time::Duration;
use async_std::future::{timeout, TimeoutError};
use rand::{thread_rng, Rng, RngCore, SeedableRng};
use async_std::task::spawn;
use std::sync::atomic::{AtomicUsize, AtomicIsize, AtomicU32, AtomicBool, AtomicU64};
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Waker, Poll};
use std::future::Future;
use std::{mem, thread, fmt};
use futures::future::BoxFuture;
use futures_test::task::{AwokenCount, new_count_waker};
use std::pin::Pin;
use std::mem::ManuallyDrop;
use futures_test::std_reexport::panic::catch_unwind;
use futures::{pin_mut, StreamExt};
use futures::poll;
use std::thread::Thread;
use futures::future::join_all;
use futures::future::poll_fn;
use async_std::task::sleep;
use std::collections::VecDeque;
use threadpool::ThreadPool;
use std::cell::{Cell, RefCell};
use futures_test::std_reexport::collections::BTreeMap;
use rand_xorshift::XorShiftRng;
use std::fmt::Debug;
use futures_test::futures_core_reexport::core_reexport::fmt::Formatter;
use crate::{Semaphore, AcquireFuture, AcquireError, SemaphoreGuard};

struct TestFuture<'a> {
    waker: Waker,
    count: AwokenCount,
    old_count: usize,
    inner: Pin<Box<AcquireFuture<'a>>>,
    amount: usize,
}

impl<'a> Debug for TestFuture<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestFuture")
            .field("waker", &(unsafe { mem::transmute::<_, &(*mut (), *mut ())>(&self.waker) }.0))
            .field("woken", &(self.count.get() != self.old_count))
            .field("inner_ptr", &(self.inner.as_ref().get_ref() as *const AcquireFuture))
            .field("amount", &self.amount)
            .field("inner", &self.inner)
            .finish()
    }
}

impl<'a> TestFuture<'a> {
    fn new(sem: &'a Semaphore, amount: usize) -> Self {
        let (waker, count) = new_count_waker();
        TestFuture { waker, count, old_count: 0, inner: Box::pin(sem.acquire(amount)), amount }
    }
    fn count(&self) -> usize {
        self.count.get()
    }
    fn poll(&mut self) -> Option<Result<SemaphoreGuard<'a>, AcquireError>> {
        match self.inner.as_mut().poll(&mut Context::from_waker(&self.waker)) {
            Poll::Pending => None,
            Poll::Ready(x) => Some(x)
        }
    }
    fn poll_if_woken(&mut self) -> Option<Result<SemaphoreGuard<'a>, AcquireError>> {
        let count = self.count.get();
        if self.old_count != count {
            self.old_count = count;
            self.poll()
        } else {
            None
        }
    }
    fn forget(self) {
        // Futures are Unpin, just not publicly.
        mem::forget(*Pin::into_inner(self.inner))
    }
}

#[test]
fn test_simple() {
    let semaphore = Semaphore::new(1);
    let mut a1 = TestFuture::new(&semaphore, 1);
    let g1 = a1.poll().unwrap().unwrap();
    let mut a2 = TestFuture::new(&semaphore, 1);
    assert_eq!(a1.count(), 0);
    assert_eq!(a2.count(), 0);
    assert!(a2.poll().is_none());
    mem::drop(g1);
    assert_eq!(a1.count(), 0);
    assert_eq!(a2.count(), 1);
    assert!(a2.poll().is_some());
}

#[test]
fn test_zero_now() {
    let semaphore = Semaphore::new(1);
    let mut a1 = TestFuture::new(&semaphore, 0);
    let g1 = a1.poll().unwrap().unwrap();
    assert_eq!(a1.count(), 0);
    mem::drop(g1);
}


#[test]
fn test_zero_pending() {
    let semaphore = Semaphore::new(0);
    println!("{:?}", semaphore);
    let mut a1 = TestFuture::new(&semaphore, 1);
    println!("{:?}", semaphore);
    assert!(a1.poll().is_none());
    println!("{:?}", semaphore);
    let mut a2 = TestFuture::new(&semaphore, 0);
    let g2 = a2.poll();
    println!("{:?}", g2);
    assert!(a2.poll().is_none());
    assert_eq!(a1.count(), 0);
    assert_eq!(a2.count(), 0);
    mem::drop(a1);
    assert_eq!(a2.count(), 1);
    assert!(a2.poll().is_some());
}

#[test]
fn test_cancel() {
    let semaphore = Semaphore::new(1);
    let mut a1 = TestFuture::new(&semaphore, 2);
    assert!(a1.poll().is_none());
    let mut a2 = TestFuture::new(&semaphore, 1);
    assert!(a2.poll().is_none());
    assert_eq!(a1.count(), 0);
    assert_eq!(a2.count(), 0);
    mem::drop(a1);
    assert_eq!(a2.count(), 1);
    assert!(a2.poll().is_some());
}

#[test]
fn test_leak() {
    let semaphore = Semaphore::new(1);
    let mut a1 = TestFuture::new(&semaphore, 2);
    assert!(a1.poll().is_none());
    a1.forget();
}

#[test]
fn test_poison_panic() {
    let semaphore = Semaphore::new(1);
    assert!(catch_unwind(
        || {
            let _guard = TestFuture::new(&semaphore, 1).poll().unwrap().unwrap();
            panic!("Expected panic");
        }
    ).is_err());
    TestFuture::new(&semaphore, 2).poll().unwrap().err().unwrap();
}

#[test]
fn test_poison_new() {
    let semaphore = Semaphore::new(usize::MAX);
    TestFuture::new(&semaphore, 2).poll().unwrap().err().unwrap();
}

#[test]
fn test_poison_release_immediate() {
    let semaphore = Semaphore::new(0);
    semaphore.release(usize::MAX);
    TestFuture::new(&semaphore, 2).poll().unwrap().err().unwrap();
}

#[test]
fn test_poison_release_add() {
    let semaphore = Semaphore::new(0);
    semaphore.release(Semaphore::MAX_AVAILABLE / 2);
    semaphore.release(Semaphore::MAX_AVAILABLE / 2 + 2);
    TestFuture::new(&semaphore, 2).poll().unwrap().err().unwrap();
}


// struct CheckedSemaphore {
//     capacity: usize,
//     semaphore: Semaphore,
//     counter: AtomicUsize,
// }
//
// impl CheckedSemaphore {
//     fn new(capacity: usize) -> Self {
//         CheckedSemaphore {
//             capacity,
//             semaphore: Semaphore::new(capacity),
//             counter: AtomicUsize::new(capacity),
//         }
//     }
//     async fn acquire(&self, amount: usize) -> SemaphoreGuard<'_> {
//         println!("Acquiring {}", amount);
//         let guard = self.semaphore.acquire(amount).await.unwrap();
//         let counter = self.counter.fetch_add(amount, Relaxed).checked_add(amount).unwrap();
//         assert!(counter <= self.capacity);
//         guard
//     }
//     fn release(&self, amount: usize) {
//         println!("Releasing {}", amount);
//         self.counter.fetch_sub(amount, Relaxed).checked_sub(amount).unwrap();
//         let result = self.semaphore.release(amount);
//         result
//     }
// }

#[test]
fn test_sequential() {
    let semaphore = Semaphore::new(0);
    let mut time = 0;
    let mut available = 0usize;
    let mut futures = BTreeMap::<usize, TestFuture>::new();
    let mut rng = XorShiftRng::seed_from_u64(954360855);
    for _ in 0..1000000 {
        if rng.gen_bool(0.1) && futures.len() < 5 {
            let amount = rng.gen_range(0, 10);
            let mut fut = TestFuture::new(&semaphore, amount);
            if let Some(guard) = fut.poll() {
                guard.unwrap().forget();
                available = available.checked_sub(amount).unwrap();
            } else {
                futures.insert(time, fut);
            }
            time += 1;
        }
        if rng.gen_bool(0.1) {
            let mut blocked = false;
            let mut ready = vec![];
            for (time, fut) in futures.iter_mut() {
                if rng.gen_bool(0.5) {
                    if let Some(guard) = fut.poll_if_woken() {
                        assert!(!blocked);
                        guard.unwrap().forget();
                        available = available.checked_sub(fut.amount).unwrap();
                        ready.push(*time);
                    } else {
                        blocked = true;
                    }
                }
            }
            for time in ready {
                futures.remove(&time);
            }
        }
        if rng.gen_bool(0.1) && available < 30 {
            //println!("{:?} {:?}", semaphore, available);
            let amount = rng.gen_range(0, 10);
            //println!("releasing {:?}", amount);
            available = available.checked_add(amount).unwrap();
            semaphore.release(amount);
            //println!("{:?} {:?}", semaphore, available);
        }
    }
}

#[test]
fn test_multicore() {
    for i in 0..100 {
        println!("iteration {:?}", i);
        test_multicore_impl();
    }
}

fn test_multicore_impl() {
    let threads = 10;
    let semaphore = Arc::new(Semaphore::new(0));
    let resource = Arc::new(AtomicIsize::new(0));
    let barrier = Arc::new(Barrier::new(threads));
    let poisoned = Arc::new(AtomicBool::new(false));
    let pending_max = Arc::new(AtomicIsize::new(-1));
    (0..threads).map(|index| thread::Builder::new().name(format!("test_multicore_impl_{}", index)).spawn({
        let semaphore = semaphore.clone();
        let resource = resource.clone();
        let barrier = barrier.clone();
        let poisoned = poisoned.clone();
        let pending_max = pending_max.clone();
        move ||  {
            let mut time = 0;
            let mut futures = BTreeMap::<usize, TestFuture>::new();
            let on_guard = |guard: Result<SemaphoreGuard, AcquireError>| {
                match guard {
                    Err(AcquireError) => {}
                    Ok(guard) => {
                        let amount = guard.forget() as isize;
                        resource.fetch_sub(amount, Relaxed).checked_sub(amount).unwrap();
                    }
                }
            };
            for _ in 0.. {
                if thread_rng().gen_bool(0.1) {
                    if futures.len() < 5 {
                        let amount = thread_rng().gen_range(0, 10);
                        let mut fut = TestFuture::new(&semaphore, amount);
                        match fut.poll() {
                            None => { futures.insert(time, fut); }
                            Some(guard) => on_guard(guard),
                        }
                        time += 1;
                    }
                }

                if thread_rng().gen_bool(0.001) {
                    if barrier.wait().is_leader() {
                        pending_max.store(-1, Relaxed);
                    }
                    let was_poisoned = poisoned.load(Relaxed);
                    let mut ready = vec![];
                    for (time, fut) in futures.iter_mut() {
                        if let Some(guard) = fut.poll_if_woken() {
                            on_guard(guard);
                            ready.push(*time);
                        }
                    }
                    for time in ready {
                        futures.remove(&time);
                    }

                    barrier.wait();
                    print!("{:?} ", futures.len());
                    if let Some(front) = futures.values_mut().next() {
                        pending_max.fetch_max(front.amount as isize, Relaxed);
                    }
                    let leader = barrier.wait();
                    let pending_amount = pending_max.load(Relaxed);
                    if pending_amount >= 0 && pending_amount <= resource.load(Relaxed) {
                        for (time, fut) in futures.iter_mut() {
                            println!("{:?} {:?}", time, fut);
                        }
                        if leader.is_leader() {
                            println!("{:?}", semaphore);
                            panic!("Should have acquired. {:?} of {:?}", pending_amount, resource.load(Relaxed));
                        }
                    }
                    if barrier.wait().is_leader() {
                        println!();
                    }
                    if was_poisoned {
                        return;
                    }
                }
                if thread_rng().gen_bool(0.1) {
                    let mut ready = vec![];
                    for (time, fut) in futures.iter_mut().rev() {
                        if thread_rng().gen_bool(0.5) {
                            let guard = if time & 1 == 1 && thread_rng().gen_bool(0.5) {
                                fut.poll()
                            } else {
                                fut.poll_if_woken()
                            };
                            if let Some(guard) = guard {
                                on_guard(guard);
                                ready.push(*time);
                            } else if !ready.is_empty() {
                                println!("{:?}", semaphore);
                                for (time, fut) in futures.iter_mut() {
                                    println!("{:?} {:?}", time, fut);
                                }
                                panic!();
                            }
                        }
                    }
                    for time in ready {
                        futures.remove(&time);
                    }
                }
                if thread_rng().gen_bool(0.1) {
                    let mut cancelled = vec![];
                    for (time, _) in futures.iter_mut().rev() {
                        if time & 2 == 2 && thread_rng().gen_bool(0.5) {
                            cancelled.push(*time);
                        }
                    }
                    for time in cancelled {
                        futures.remove(&time);
                    }
                }
                if thread_rng().gen_bool(0.1) && resource.load(Relaxed) < 20 {
                    let amount = thread_rng().gen_range(0, 20);
                    resource.fetch_add(amount as isize, Relaxed);
                    semaphore.release(amount);
                }
                if thread_rng().gen_bool(0.1) {
                    if let Ok(guard)
                    = semaphore.try_acquire(thread_rng().gen_range(0, 10)) {
                        on_guard(Ok(guard));
                    }
                }
                if thread_rng().gen_bool(0.0001) {
                    poisoned.store(true, Relaxed);
                    semaphore.poison();
                }
            }
        }
    }).unwrap()).collect::<Vec<_>>().into_iter().for_each(|x| x.join().unwrap());
}
