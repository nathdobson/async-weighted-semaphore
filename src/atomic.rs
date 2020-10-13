use std::sync::atomic::{Ordering, AtomicUsize};
use std::marker::PhantomData;
use std::{mem};


/// An AtomicUsize containing a bitpacked `T` .
pub struct Atomic<T: Packable>(AtomicUsize, PhantomData<T>);

/// Specify how to bitpack a value.
pub trait Packable: Sized + Copy {
    unsafe fn encode(val: Self) -> usize;
    unsafe fn decode(val: usize) -> Self;
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
    pub fn cmpxchg_weak_acqrel(&self, current: &mut T, new: T) -> bool {
        unsafe {
            match self.0.compare_exchange_weak(
                T::encode(*current), T::encode(new),
                Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => true,
                Err(next) => { *current = T::decode(next); false}
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
