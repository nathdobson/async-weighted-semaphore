use crate::{Waiter, Semaphore};
use crate::atomic::Packable;
use crate::state::AcquireState::{Available, Queued};
use crate::state::ReleaseState::{LockedDirty, Locked, Unlocked};

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub enum ReleaseState {
    // Indicates there are no releases in progress. Contains a count of permits that are available
    // to the next pending acquire.
    Unlocked(usize),
    // Indicates there is at least one release in progress. One release owns the lock and tracks
    // the number of available permits.
    Locked,
    // Indicates there is at least one release in progress, and a release completed without holding
    // the lock, defering a number of permits for the release lock owner.
    LockedDirty(usize),
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub(crate) enum AcquireState {
    // Indicates that there may be pending acquires and contains a pointer to the back of the queue.
    Queued(*const Waiter),
    // Indicates that there are not pending acquires and contains the available permits.
    Available(usize),
}

impl Packable for ReleaseState {
    unsafe fn encode(val: Self) -> usize {
        match val {
            Unlocked(releasable) => {
                assert!(releasable <= Semaphore::MAX_AVAILABLE);
                releasable
            }
            Locked => !0,
            LockedDirty(releasable) => {
                assert!(releasable <= Semaphore::MAX_AVAILABLE);
                releasable | (1 << 63)
            }
        }
    }
    unsafe fn decode(val: usize) -> Self {
        if val == !0 {
            Locked
        } else {
            let releasable = val & !(1 << 63);
            if val & (1 << 63) != 0 {
                LockedDirty(releasable)
            } else {
                Unlocked(releasable)
            }
        }
    }
}

impl Packable for AcquireState {
    unsafe fn encode(val: Self) -> usize {
        match val {
            Queued(back) => back as usize,
            Available(available) => ((available << 1) | 1) as usize,
        }
    }
    unsafe fn decode(val: usize) -> Self {
        if val & 1 == 1 {
            Available((val >> 1) as usize)
        } else {
            Queued(val as *const Waiter)
        }
    }
}
