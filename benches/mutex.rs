#![feature(test)]
extern crate test;

use test::Bencher;
use std::sync::{Mutex as SyncMutex, Arc};
use async_std::sync::Mutex as AsyncStdMutex;
use futures_locks::Mutex as FuturesMutex;
use tokio::sync::Mutex as TokioMutex;
use futures::executor::block_on;
use async_std::task::spawn;
use futures_test::std_reexport::sync::atomic::AtomicUsize;
use futures_test::std_reexport::sync::atomic::Ordering::SeqCst;
use async_weighted_semaphore::Semaphore;
use std::cell::UnsafeCell;
use std::thread;
use futures::future::BoxFuture;
use futures::executor::ThreadPool;
use futures::task::SpawnExt;
use futures::future::join_all;

const PARALLELISM: usize = 8;
const CONCURRENCY: usize = 100;
const ITEMS: usize = 2;
const ITERS: usize = 1000;

fn get_loop<'a, T>(v: &'a Vec<Arc<T>>) -> impl Iterator<Item=&'a T> {
    (0..ITERS).map(move |i| &*v[i % ITEMS])
}

trait AtomicCounter: Default + 'static + Send + Sync {
    fn do_loop<'a>(items: Vec<Arc<Self>>) -> BoxFuture<'static, ()>;
}

impl AtomicCounter for AtomicUsize {
    fn do_loop<'a>(items: Vec<Arc<Self>>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for x in get_loop(&items) {
                x.fetch_add(1, SeqCst);
            }
        })
    }
}

impl AtomicCounter for SyncMutex<usize> {
    fn do_loop<'a>(items: Vec<Arc<Self>>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for x in get_loop(&items) {
                *x.lock().unwrap() += 1;
            }
        })
    }
}

impl AtomicCounter for AsyncStdMutex<usize> {
    fn do_loop<'a>(items: Vec<Arc<Self>>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for x in get_loop(&items) {
                *x.lock().await += 1;
            }
        })
    }
}

impl AtomicCounter for TokioMutex<usize> {
    fn do_loop<'a>(items: Vec<Arc<Self>>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for x in get_loop(&items) {
                *x.lock().await += 1;
            }
        })
    }
}

impl AtomicCounter for FuturesMutex<usize> {
    fn do_loop<'a>(items: Vec<Arc<Self>>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for x in get_loop(&items) {
                *x.lock().await += 1;
            }
        })
    }
}

struct SemMutex(Semaphore, UnsafeCell<usize>);

unsafe impl Send for SemMutex {}

unsafe impl Sync for SemMutex {}

impl Default for SemMutex {
    fn default() -> Self {
        SemMutex(Semaphore::new(1), UnsafeCell::new(0))
    }
}

impl AtomicCounter for SemMutex {
    fn do_loop<'a>(items: Vec<Arc<Self>>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for x in get_loop(&items) {
                let _guard = x.0.acquire(1).await.unwrap();
                unsafe { *x.1.get() += 1; }
            }
        })
    }
}

fn run_impl<T: AtomicCounter>(bencher: &mut Bencher) {
    let items = (0..ITEMS).map(|_| Arc::new(T::default())).collect::<Vec<_>>();
    let pool = ThreadPool::builder().pool_size(PARALLELISM).create().unwrap();
    bencher.iter(|| {
        block_on(join_all((0..CONCURRENCY).map(|_| {
            let items = items.clone();
            pool.spawn_with_handle(T::do_loop(items)).unwrap()
        }).collect::<Vec<_>>().into_iter()));
    });
}

#[bench]
fn run_atomic(bencher: &mut Bencher) {
    run_impl::<AtomicUsize>(bencher);
}

#[bench]
fn run_sync_mutex(bencher: &mut Bencher) {
    run_impl::<SyncMutex<usize>>(bencher);
}

#[bench]
fn run_async_std_mutex(bencher: &mut Bencher) {
    run_impl::<AsyncStdMutex<usize>>(bencher);
}

#[bench]
fn run_futures_mutex(bencher: &mut Bencher) {
    run_impl::<FuturesMutex<usize>>(bencher);
}

#[bench]
fn run_tokio_mutex(bencher: &mut Bencher) {
    run_impl::<TokioMutex<usize>>(bencher);
}

#[bench]
fn run_sem_mutex(bencher: &mut Bencher) {
    run_impl::<SemMutex>(bencher);
}