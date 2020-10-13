use crate::Semaphore;
use crate::state::{Permits, Waiter};
use crate::state::ReleaseState::{Unlocked, Locked, LockedDirty};
use crate::state::AcquireState::{Queued, Available};
use std::ptr::null;
use crate::waker::{AtomicWaker, WakerResult};
use std::sync::atomic::Ordering::{AcqRel, Acquire};

// The state of a single call to release.
pub struct ReleaseAction<'a> {
    pub sem: &'a Semaphore,
    pub releasable: Permits,
}

impl<'a> ReleaseAction<'a> {
    // Attempt to acquire the release lock. If it is locked, defer the release for the lock owner
    // by including the new permits in ReleaseState.
    pub unsafe fn lock_or_else_defer(&mut self) -> bool {
        let mut current = self.sem.release.load(Acquire);
        loop {
            match current {
                Unlocked(available) => {
                    if self.sem.release.cmpxchg_weak_acqrel(&mut current, Locked) {
                        self.releasable += available;
                        return true;
                    }
                }
                Locked => {
                    if self.sem.release.cmpxchg_weak_acqrel(&mut current, LockedDirty(self.releasable)) {
                        return false;
                    }
                }
                LockedDirty(available) => {
                    if self.sem.release.cmpxchg_weak_acqrel(&mut current, LockedDirty(available + self.releasable)) {
                        return false;
                    }
                }
            }
        }
    }
    // If the queue is empty, make the permits available in AcquireState. Otherwise take all nodes
    // in AcquireState and add next edges.
    unsafe fn flip(&mut self) {
        let mut current = self.sem.acquire.load(Acquire);
        loop {
            match current {
                Queued(back) if back == null() && *self.sem.front.get() == null() => {
                    if self.sem.acquire.cmpxchg_weak_acqrel(&mut current, Available(self.releasable)) {
                        self.releasable = Permits::new(0);
                        break;
                    }
                }
                Available(available) => {
                    if self.sem.acquire.cmpxchg_weak_acqrel(&mut current, Available(available + self.releasable)) {
                        assert!(*self.sem.front.get() == null());
                        self.releasable = Permits::new(0);
                        break;
                    }
                }
                Queued(back) => {
                    if back == null() {
                        break;
                    }
                    if self.sem.acquire.cmpxchg_weak_acqrel(&mut current, Queued(null())) {
                        let mut waiter = back;
                        while waiter != null() {
                            let prev = *(*waiter).prev.get();
                            if prev == null() {
                                let middle = *self.sem.middle.get();
                                if middle == null() {
                                    *self.sem.front.get() = waiter;
                                    *self.sem.middle.get() = waiter;
                                } else {
                                    *(*middle).next.get() = waiter;
                                    *(*waiter).prev.get() = middle;
                                }
                                break;
                            }
                            *(*prev).next.get() = waiter;
                            waiter = prev;
                        }
                        *self.sem.middle.get() = back;
                        break;
                    }
                }
            }
        }
    }

    // Clear the cancellation queue, removing nodes from the finish queue. Nodes should have next edges.
    unsafe fn cancel(&mut self, mut next_cancel: *const Waiter) {
        while next_cancel != null() {
            let next = *(*next_cancel).next.get();
            let prev = *(*next_cancel).prev.get();
            if prev == null() {
                *self.sem.front.get() = next;
            } else {
                *(*prev).next.get() = next;
            }
            if next == null() {
                *self.sem.middle.get() = prev;
            } else {
                *(*next).prev.get() = prev;
            }
            let waker = &(*next_cancel).waker as *const AtomicWaker;
            next_cancel = *(*next_cancel).next_cancel.get();
            AtomicWaker::accept_cancel(waker);
        }
    }

    // Try to finish and pop the next node.
    unsafe fn try_pop(&mut self) -> bool {
        let front = *self.sem.front.get();
        let amount = (*front).amount;
        let next = *(*front).next.get();
        let releasable = self.releasable.into_usize();
        if let Some(releasable) = releasable {
            if releasable < (*front).amount {
                return false;
            }
        }
        match AtomicWaker::finish(&(*front).waker, releasable.is_none()) {
            WakerResult::Cancelling =>
                return false,
            WakerResult::Finished { .. } => {
                if let Some(releasable) = releasable {
                    self.releasable = Permits::new(releasable - amount);
                }
                *self.sem.front.get() = next;
                if next == null() {
                    *self.sem.middle.get() = null();
                } else {
                    *(*next).prev.get() = null();
                }
                return true;
            }
        }
    }

    // Attempt to unlock the release lock. If there were concurrent releases or cancels, keep the
    // lock in order to complete those operations.
    unsafe fn try_unlock(&mut self) -> bool {
        let mut current = self.sem.release.load(Acquire);
        loop {
            match current {
                LockedDirty(available) => {
                    if self.sem.release.cmpxchg_weak_acqrel(&mut current, Locked) {
                        self.releasable += available;
                        return false;
                    }
                }
                Unlocked(_) => unreachable!(),
                Locked => {
                    if self.sem.release.cmpxchg_weak_acqrel(&mut current, Unlocked(self.releasable)) {
                        return true;
                    }
                }
            }
        }
    }

    // Perform a release operation
    pub unsafe fn release(&mut self) {
        if !self.lock_or_else_defer() {
            return;
        }
        loop {
            // Take the cancel stack first to ensure all cancelled nodes have next edges.
            let next_cancel = self.sem.next_cancel.swap(null(), AcqRel);
            // Add next edges
            self.flip();
            // Clear the cancellation queue.
            self.cancel(next_cancel);
            // Finish nodes until stuck.
            while *self.sem.front.get() != null() && self.try_pop() {}
            // Unlock if there were no concurrent operations.
            if self.try_unlock() {
                return;
            }
        }
    }
}