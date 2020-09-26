use std::{mem};
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::Ordering;
use std::sync::atomic::Ordering::{AcqRel, Acquire};

use ::atomic::Atomic as RawAtomic;

pub struct Atomic<T: Packable>(RawAtomic<T::Raw>);

pub trait Packable: Sized + Copy {
    type Raw: Copy;
    unsafe fn encode(val: Self) -> Self::Raw {
        force_transmute(val)
    }
    unsafe fn decode(val: Self::Raw) -> Self {
        force_transmute(val)
    }
}

pub unsafe fn force_transmute<T, U>(value: T) -> U {
    assert_eq!(size_of::<T>(), size_of::<U>());
    let result = mem::transmute_copy(&value);
    mem::forget(value);
    result
}

#[must_use]
pub struct Transact<'a, T: Packable> {
    atom: &'a Atomic<T>,
    current: T::Raw,
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
    pub fn commit(self) -> Result<T, T::Raw> {
        unsafe {
            match self.atom.0.compare_exchange_weak(
                self.current, T::encode(self.new),
                AcqRel, Acquire) {
                Err(e) => Err(e),
                Ok(_) => Ok(self.new),
            }
        }
    }
}

impl<T: Packable> Atomic<T> {
    pub fn new(val: T) -> Self {
        Atomic(RawAtomic::new(unsafe { T::encode(val) }))
    }
    pub fn load(&self, order: Ordering) -> T {
        unsafe { T::decode(self.0.load(order)) }
    }
    // pub fn compare_and_swap(
    //     &self,
    //     current: T,
    //     new: T,
    //     order: Ordering)
    //     -> T {
    //     unsafe {
    //         match self.0.compare_exchange(
    //             T::encode(current),
    //             T::encode(new),
    //             order,
    //             match order {
    //                 Relaxed | Release => Relaxed,
    //                 Acquire | AcqRel => Acquire,
    //                 SeqCst => SeqCst,
    //                 _ => unimplemented!(),
    //             }) {
    //             Ok(old) => T::decode(old),
    //             Err(old) => T::decode(old),
    //         }
    //     }
    // }

    // pub fn swap(&self, new: T, order: Ordering) -> T {
    //     unsafe {
    //         T::decode(self.0.swap(T::encode(new), order))
    //     }
    // }

    pub fn transact<'a, R>(&'a self, mut update: impl FnMut(Transact<'a, T>) -> Result<R, T::Raw>) -> R {
        unsafe {
            let mut value = self.0.load(Acquire);
            loop {
                match update(Transact {
                    atom: self,
                    current: value,
                    new: T::decode(value),
                }) {
                    Err(e) => value = e,
                    Ok(v) => return v,
                }
            }
        }
    }
}