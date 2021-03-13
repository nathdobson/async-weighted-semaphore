
use std::sync::{Arc, Barrier, Mutex};


use rand::{thread_rng, Rng, SeedableRng};

use std::sync::atomic::{AtomicIsize, AtomicBool};
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Waker, Poll};
use std::future::Future;
use std::{mem, thread, fmt};

use futures::FutureExt;
use futures_test::task::{AwokenCount, new_count_waker};
use std::pin::Pin;
use std::panic::catch_unwind;
use std::collections::BTreeMap;
use rand_xorshift::XorShiftRng;
use std::fmt::Debug;
use std::fmt::Formatter;
use crate::{Semaphore, AcquireFuture, PoisonError, SemaphoreGuard, SemaphoreGuardArc};


struct TestFuture<'a> {
    waker: Waker,
    count: AwokenCount,
    old_count: usize,
    inner: Pin<Box<AcquireFuture<'a>>>,
    amount: usize,
}

impl<'a> Debug for TestFuture<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TF")
            //.field("waker", &(unsafe { mem::transmute::<_, &(*mut (), *mut ())>(&self.waker) }.0))
            .field("w", &(self.count.get() != self.old_count))
            .field("p", &(self.inner.as_ref().get_ref() as *const AcquireFuture))
            //.field("amount", &self.amount)
            .field("i", &self.inner)
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
    fn poll(&mut self) -> Option<Result<SemaphoreGuard<'a>, PoisonError>> {
        match self.inner.as_mut().poll(&mut Context::from_waker(&self.waker)) {
            Poll::Pending => None,
            Poll::Ready(x) => Some(x)
        }
    }
    fn poll_if_woken(&mut self) -> Option<Result<SemaphoreGuard<'a>, PoisonError>> {
        let count = self.count.get();
        if self.old_count != count {
            self.old_count = count;
            self.poll()
        } else {
            None
        }
    }
    fn into_inner(self) -> Pin<Box<AcquireFuture<'a>>> {
        self.inner
    }
}

#[test]
fn test_simple() {
    // Binary semaphore should be exclusive:
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
    println!("A {:?}", semaphore);
    let mut a1 = TestFuture::new(&semaphore, 1);
    println!("B {:?}", semaphore);
    assert!(a1.poll().is_none());
    println!("C {:?}", semaphore);
    let mut a2 = TestFuture::new(&semaphore, 0);
    let _g2 = a2.poll();
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
fn test_extend() {
    let semaphore = Semaphore::new(15);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
    let mut g1 = TestFuture::new(&semaphore, 10).poll().unwrap().unwrap();
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(5));
    let g2 = TestFuture::new(&semaphore, 5).poll().unwrap().unwrap();
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(0));
    g1.extend(g2);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(0));
    drop(g1);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
}

// Attempting to combine guards will poison the semaphores if the permits would overflow
#[test]
fn test_extend_overflow() {
    let semaphore = Semaphore::new(Semaphore::MAX_AVAILABLE);
    let mut g1 = TestFuture::new(&semaphore, Semaphore::MAX_AVAILABLE).poll().unwrap().unwrap();
    let g2 = SemaphoreGuard::new(&semaphore, 1);
    g1.extend(g2);
    drop(g1);
    // At this point, the semaphore should be poisoned
    TestFuture::new(&semaphore, 1).poll().unwrap().err().unwrap();
}

// Atempting to combine guards from different semaphores should poison both
#[test]
#[should_panic(expected = "Can't extend a guard with a guard from a different Semaphore")]
fn test_extend_wrong_sems() {
    let semaphore1 = Semaphore::new(15);
    let semaphore2 = Semaphore::new(15);
    let mut g1 = TestFuture::new(&semaphore1, 10).poll().unwrap().unwrap();
    let g2 = TestFuture::new(&semaphore2, 5).poll().unwrap().unwrap();
    g1.extend(g2);
}

#[test]
fn test_extend_arc() {
    let semaphore = Arc::new(Semaphore::new(15));
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
    let mut g1 = semaphore.acquire_arc(10).now_or_never().unwrap().unwrap();
    let g2 = semaphore.acquire_arc(5).now_or_never().unwrap().unwrap();
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(0));
    g1.extend(g2);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(0));
    drop(g1);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
}

#[test]
#[should_panic(expected = "Can't extend a guard with a guard from a different Semaphore")]
fn test_extend_arc_wrong_sems() {
    let semaphore1 = Arc::new(Semaphore::new(15));
    let semaphore2 = Arc::new(Semaphore::new(15));
    let mut g1 = semaphore1.acquire_arc(10).now_or_never().unwrap().unwrap();
    let g2 = semaphore2.acquire_arc(5).now_or_never().unwrap().unwrap();
    g1.extend(g2);
}

// Attempting to combine guards will poison the semaphores if the permits would overflow
#[test]
fn test_extend_arc_overflow() {
    let semaphore = Arc::new(Semaphore::new(Semaphore::MAX_AVAILABLE));
    let mut g1 = semaphore.acquire_arc(Semaphore::MAX_AVAILABLE).now_or_never().unwrap().unwrap();
    let g2 = SemaphoreGuardArc::new(semaphore.clone(), 1);
    g1.extend(g2);
    drop(g1);
    // At this point, the semaphore should be poisoned
    assert!(semaphore.acquire_arc(1).now_or_never().unwrap().is_err());
}

#[test]
fn test_split() {
    let semaphore = Semaphore::new(15);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
    let mut g1 = TestFuture::new(&semaphore, 15).poll().unwrap().unwrap();
    let g2 = g1.split(5).unwrap();
    drop(g2);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(5));
    drop(g1);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
}

#[test]
fn test_split_underflow() {
    let semaphore = Semaphore::new(15);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
    let mut g1 = TestFuture::new(&semaphore, 5).poll().unwrap().unwrap();
    assert!(g1.split(10).is_err());
}

#[test]
fn test_split_arc() {
    let semaphore = Arc::new(Semaphore::new(15));
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
    let mut g1 = semaphore.acquire_arc(15).now_or_never().unwrap().unwrap();
    let g2 = g1.split(5).unwrap();
    drop(g2);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(5));
    drop(g1);
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
}

#[test]
fn test_split_arc_underflow() {
    let semaphore = Arc::new(Semaphore::new(15));
    assert_eq!(semaphore.acquire.load(Relaxed).available(), Some(15));
    let mut g1 = semaphore.acquire_arc(5).now_or_never().unwrap().unwrap();
    assert!(g1.split(10).is_err());
}

#[test]
fn test_leak() {
    let semaphore = Semaphore::new(1);
    let mut a1 = TestFuture::new(&semaphore, 2);
    assert!(a1.poll().is_none());
    lazy_static! {
        static ref SUPPRESS: Mutex<usize> = Mutex::new(0);
    }
    unsafe { *SUPPRESS.lock().unwrap() = Box::into_raw(Pin::into_inner_unchecked(a1.into_inner())) as usize; }
    mem::drop(semaphore);
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

#[test]
fn test_poison_release_concurrent() {
    let semaphore = Semaphore::new(0);
    let mut future = TestFuture::new(&semaphore, 2);
    assert!(future.poll().is_none());
    semaphore.release(usize::MAX);
    future.poll_if_woken().expect("done").err().expect("AcquireError");
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
    for _ in 0..100000 {
        //println!();
        // println!("{:?}", semaphore);
        // for f in futures.iter() {
        //     println!("{:?}", f);
        // }
        if rng.gen_bool(0.1) && futures.len() < 5 {
            let amount = rng.gen_range(0, 10);
            //println!("acquiring {:?}", amount);
            let mut fut = TestFuture::new(&semaphore, amount);
            if let Some(guard) = fut.poll() {
                guard.unwrap().forget();
                available = available.checked_sub(amount).unwrap();
            } else {
                futures.insert(time, fut);
            }
            time += 1;
        } else if rng.gen_bool(0.1) {
            let mut blocked = false;
            let mut ready = vec![];
            //println!("polling");
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
        } else if rng.gen_bool(0.1) && available < 30 {
            let amount = rng.gen_range(0, 10);
            //println!("releasing {:?}", amount);
            available = available.checked_add(amount).unwrap();
            semaphore.release(amount);
        }
    }
}

#[test]
fn test_parallel() {
    for i in 0..1000 {
        println!("iteration {:?}", i);
        test_parallel_impl();
    }
}

fn test_parallel_impl() {
    let threads = 10;
    let semaphore = Arc::new(Semaphore::new(0));
    let resource = Arc::new(AtomicIsize::new(0));
    let barrier = Arc::new(Barrier::new(threads));
    let poisoned = Arc::new(AtomicBool::new(false));
    let pending_max = Arc::new(AtomicIsize::new(-1));
    (0..threads).map(|index| thread::Builder::new().name(format!("test_parallel_impl_{}", index)).spawn({
        let semaphore = semaphore.clone();
        let resource = resource.clone();
        let barrier = barrier.clone();
        let poisoned = poisoned.clone();
        let pending_max = pending_max.clone();
        move || {
            let mut time = 0;
            let mut futures = BTreeMap::<usize, TestFuture>::new();
            let on_guard = |guard: Result<SemaphoreGuard, PoisonError>| {
                match guard {
                    Err(PoisonError) => {}
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
