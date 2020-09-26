use futures::executor::{LocalPool, ThreadPool, block_on};
use futures::task::{LocalSpawnExt, SpawnExt};
use std::sync::{Arc, Mutex};
use std::rc::Rc;
use std::mem;
use std::time::Duration;
use async_std::future::timeout;
use crate::{Semaphore, SemaphoreGuard};
use rand::{thread_rng, Rng};

#[test]
fn test_simple() {
    println!("A");
    let semaphore = Rc::new(Semaphore::new(10));
    println!("B");
    let mut pool = LocalPool::new();
    println!("C");
    let spawner = pool.spawner();
    println!("D");
    spawner.spawn_local({
        println!("E");
        let semaphore = semaphore.clone();
        async move {
            println!("F");
            semaphore.acquire(10).await.forget();
            println!("G");
            semaphore.acquire(10).await.forget();
            println!("H");
        }
    }).unwrap();
    println!("I");
    pool.run_until_stalled();
    println!("J");
    semaphore.release(10);
    println!("K");
    pool.run();
    println!("L");
}

struct CheckedSemaphore {
    capacity: usize,
    semaphore: Semaphore,
    counter: Mutex<usize>,
}

impl CheckedSemaphore {
    fn new(capacity: usize) -> Self {
        CheckedSemaphore {
            capacity,
            semaphore: Semaphore::new(capacity),
            counter: Mutex::new(0),
        }
    }
    async fn acquire(&self, amount: usize) -> SemaphoreGuard<'_> {
        //println!("+ {}", amount);
        let guard = self.semaphore.acquire(amount).await;
        let mut lock = self.counter.lock().unwrap();
        //println!("{} + {} = {} ", *lock, amount, *lock + amount);
        *lock += amount;
        assert!(*lock <= self.capacity);
        mem::drop(lock);
        //println!("{:?}", self.semaphore);
        guard
    }
    fn release(&self, amount: usize) {
        let mut lock = self.counter.lock().unwrap();
        assert!(*lock >= amount);
        //println!("{} - {} = {} ", *lock, amount, *lock - amount);
        *lock -= amount;
        mem::drop(lock);
        let result = self.semaphore.release(amount);
        //println!("{:?}", self.semaphore);
        result
    }
}

#[test]
fn test_multicore() {
    for i in 0..10 {
        println!("{:?}", i);
        test_multicore_impl();
    }
}

fn test_multicore_impl() {
    let capacity = 100;
    let semaphore = Arc::new(CheckedSemaphore::new(capacity));
    let pool = ThreadPool::builder().pool_size(10).create().unwrap();
    (0..100).map(|_thread|
        pool.spawn_with_handle({
            let semaphore = semaphore.clone();
            async move {
                //let indent = " ".repeat(thread * 10);
                let mut owned = 0;
                for _i in 0..100 {
                    //println!("{:?}", semaphore.semaphore);
                    if owned == 0 {
                        owned = thread_rng().gen_range(0, capacity + 1);
                        //println!("{} : acquiring {}", thread, owned);
                        let dur = Duration::from_millis(thread_rng().gen_range(0, 10));
                        if let Ok(guard) =
                        timeout(dur, semaphore.acquire(owned)).await {
                            guard.forget();
                        } else {
                            owned = 0;
                        }
                    } else {
                        let mut rng = thread_rng();
                        let r = if rng.gen_bool(0.5) {
                            owned
                        } else {
                            rng.gen_range(1, owned + 1)
                        };
                        owned -= r;
                        semaphore.release(r);
                    }
                }
                semaphore.release(owned);
            }
        }).unwrap()
    ).collect::<Vec<_>>().into_iter().for_each(block_on);
    mem::drop(pool);
    assert_eq!(Arc::strong_count(&semaphore), 1);
}
