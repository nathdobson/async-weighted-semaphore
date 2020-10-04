use crate::{Semaphore, AcquireError, SemaphoreGuard};
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::thread;
use std::sync::Arc;
use futures::executor::block_on;


#[derive(Debug)]
pub struct RwLock<T: ?Sized> {
    semaphore: Semaphore,
    inner: UnsafeCell<T>,
}

pub struct RwLockReadGuard<'a, T: ?Sized> {
    _guard: SemaphoreGuard<'a>,
    value: *const T,
}

pub struct RwLockWriteGuard<'a, T: ?Sized> {
    _guard: SemaphoreGuard<'a>,
    value: *mut T,
}

impl<T: ?Sized> RwLock<T> {
    fn new(inner: T) -> Self where T: Sized {
        RwLock {
            semaphore: Semaphore::new(Semaphore::MAX_AVAILABLE),
            inner: UnsafeCell::new(inner),
        }
    }
    async fn read(&self) -> Result<RwLockReadGuard<'_, T>, AcquireError> {
        Ok(RwLockReadGuard { _guard: self.semaphore.acquire(1).await?, value: self.inner.get() })
    }
    async fn write(&self) -> Result<RwLockWriteGuard<'_, T>, AcquireError> {
        Ok(RwLockWriteGuard { _guard: self.semaphore.acquire(Semaphore::MAX_AVAILABLE).await?, value: self.inner.get() })
    }
    fn into_inner(self) -> Result<T, AcquireError> where T: Sized {
        match self.semaphore.try_acquire(0) {
            Ok(_) => Ok(self.inner.into_inner()),
            Err(_) => Err(AcquireError),
        }
    }
}

unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}

unsafe impl<T: ?Sized + Sync + Send> Sync for RwLock<T> {}

unsafe impl<'a, T: ?Sized + Sync> Send for RwLockReadGuard<'a, T> {}

unsafe impl<'a, T: ?Sized + Sync> Sync for RwLockReadGuard<'a, T> {}

unsafe impl<'a, T: ?Sized + Sync + Send> Send for RwLockWriteGuard<'a, T> {}

unsafe impl<'a, T: ?Sized + Sync + Send> Sync for RwLockWriteGuard<'a, T> {}

impl<'a, T: ?Sized> Deref for RwLockReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.value }
    }
}

impl<'a, T: ?Sized> Deref for RwLockWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.value }
    }
}

impl<'a, T: ?Sized> DerefMut for RwLockWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.value }
    }
}


#[test]
fn test() {
    let lock = Arc::new(RwLock::new(Vec::new()));
    let threads = 10;
    let iters = 500;
    (0..threads).map(|thread| {
        let lock = lock.clone();
        thread::spawn(move || {
            block_on(async {
                for i in (thread..iters * threads + thread).step_by(threads) {
                    while lock.read().await.unwrap().len() != i {}
                    lock.write().await.unwrap().push(i)
                }
            })
        })
    }).collect::<Vec<_>>().into_iter().for_each(|x| x.join().unwrap());
    let vec = Arc::try_unwrap(lock).unwrap().into_inner().unwrap();
    assert_eq!(vec, (0..threads*iters).collect::<Vec<_>>());
}