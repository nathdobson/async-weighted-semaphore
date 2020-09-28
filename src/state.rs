use crate::{Waiter, Semaphore};
use crate::atomic::Packable;
use crate::state::AcquireState::{Available, Queued};
use crate::state::ReleaseState::{LockedDirty, Locked, Unlocked};
use std::ops::{AddAssign, Add, Sub, SubAssign};

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub struct Permits(pub(self) usize);

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub enum ReleaseState {
    // Indicates there are no releases in progress. Contains a count of permits that are available
    // to the next pending acquire.
    Unlocked(Permits),
    // Indicates there is at least one release in progress. One release owns the lock and tracks
    // the number of available permits.
    Locked,
    // Indicates there is at least one release in progress, and a release completed without holding
    // the lock, defering a number of permits for the release lock owner.
    LockedDirty(Permits),
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub(crate) enum AcquireState {
    // Indicates that there may be pending acquires and contains a pointer to the back of the queue.
    Queued(*const Waiter),
    // Indicates that there are not pending acquires and contains the available permits.
    Available(Permits),
}

impl Packable for ReleaseState {
    unsafe fn encode(val: Self) -> usize {
        match val {
            Locked => 0,
            Unlocked(releasable) => match releasable.into_usize() {
                None => 1,
                Some(releasable) => (releasable << 3) | 2
            },
            LockedDirty(releasable) => match releasable.into_usize() {
                None => 3,
                Some(releasable) => (releasable << 3) | 4
            },
        }
    }
    unsafe fn decode(val: usize) -> Self {
        match val & 7 {
            0 => Locked,
            1 => Unlocked(Permits::poison()),
            2 => Unlocked(Permits::new(val >> 3)),
            3 => LockedDirty(Permits::poison()),
            4 => LockedDirty(Permits::new(val >> 3)),
            _ => unreachable!()
        }
    }
}

impl Packable for AcquireState {
    unsafe fn encode(val: Self) -> usize {
        match val {
            Queued(back) => back as usize,
            Available(available) => match available.into_usize() {
                None => usize::max_value(),
                Some(available) => ((available << 1) | 1) as usize,
            }
        }
    }
    unsafe fn decode(val: usize) -> Self {
        if val & 1 == 1 {
            if val == usize::MAX {
                Available(Permits::poison())
            } else {
                Available(Permits::new((val >> 1) as usize))
            }
        } else {
            Queued(val as *const Waiter)
        }
    }
}

impl Permits {
    pub fn new(x: usize) -> Self {
        Permits(x)
    }
    pub fn poison() -> Self {
        Permits(usize::max_value())
    }
    pub fn into_usize(self) -> Option<usize> {
        if self.0 <= Semaphore::MAX_AVAILABLE {
            Some(self.0)
        } else {
            None
        }
    }
}

impl Add for Permits {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Permits(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Permits {
    type Output = Permits;
    fn sub(self, rhs: Permits) -> Self::Output {
        match self.into_usize() {
            None => self,
            Some(this) => Permits(this - rhs.0),
        }
    }
}

impl AddAssign for Permits {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl SubAssign for Permits {
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}