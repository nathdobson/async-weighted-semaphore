use std::ops::{Deref, DerefMut};
use std::sync::atomic::{Ordering, AtomicUsize};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
use std::marker::PhantomData;
use std::{mem, thread};
use std::thread::Thread;
use std::time::Duration;

/// An AtomicUsize containing a bitpacked `T` .
pub struct Atomic<T: Packable>(AtomicUsize, PhantomData<T>);

/// Specify how to bitpack a value.
pub trait Packable: Sized + Copy {
    unsafe fn encode(val: Self) -> usize;
    unsafe fn decode(val: usize) -> Self;
}

/// An attempt to perform a read-modify-write operation on an Atomic.
#[must_use]
pub struct Transact<'a, T: Packable> {
    atom: &'a Atomic<T>,
    current: usize,
    new: T,
}

impl<'a, T: Packable> Deref for Transact<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.new
    }
}

impl<'a, T: Packable> DerefMut for Transact<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.new
    }
}

impl<'a, T: Packable> Transact<'a, T> {
    /// Try to commit a transaction to an atomic variable. Return an Err containing the current
    /// value on error.
    pub fn commit(self) -> Result<T, T> {
        unsafe {
            match self.atom.0.compare_exchange_weak(
                self.current, T::encode(self.new),
                AcqRel, Acquire) {
                Err(e) => Err(T::decode(e)),
                Ok(_) => Ok(self.new),
            }
        }
    }
}

impl<T: Packable> Atomic<T> {
    pub fn new(val: T) -> Self {
        Atomic(AtomicUsize::new(unsafe { T::encode(val) }), PhantomData)
    }
    pub fn load(&self, order: Ordering) -> T {
        unsafe { T::decode(self.0.load(order)) }
    }
    pub fn store(&self, val: T, order: Ordering) {
        unsafe { self.0.store(T::encode(val), order); }
    }
    pub fn swap(&self, val: T, order: Ordering) -> T {
        unsafe { T::decode(self.0.swap(T::encode(val), order)) }
    }
    /// Perform a transaction (similar fetch_update).
    /// Calls the callback with the current value as a Transact. Callers may mutate this Transact
    /// and commit it. Errors from commit should be raised with '?'.
    pub fn transact<'a, R>(&'a self, mut update: impl FnMut(Transact<'a, T>) -> Result<R, T>) -> R {
        unsafe {
            let mut value = self.0.load(Acquire);
            loop {
                match update(Transact {
                    atom: self,
                    current: value,
                    new: T::decode(value),
                }) {
                    Err(e) => value = T::encode(e),
                    Ok(v) => return v,
                }
            }
        }
    }
}

impl<T> Packable for *const T {
    unsafe fn encode(val: Self) -> usize {
        mem::transmute(val)
    }
    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val)
    }
}

impl<T> Packable for *mut T {
    unsafe fn encode(val: Self) -> usize {
        mem::transmute(val)
    }
    unsafe fn decode(val: usize) -> Self {
        mem::transmute(val)
    }
}
