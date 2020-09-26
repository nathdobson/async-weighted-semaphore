use crate::Waiter;
use crate::atomic::Packable;
use crate::state::ReleaseMode::{Unlocked, Locked, LockedDirty};
use crate::state::AcquireState::{Available, Queued};

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub enum ReleaseMode {
    Unlocked,
    Locked,
    LockedDirty,
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub struct ReleaseState {
    pub releasable: usize,
    pub mode: ReleaseMode,
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub enum AcquireState {
    Queued(*const Waiter),
    Available(usize),
}

impl Packable for ReleaseState {
    type Raw = usize;

    unsafe fn encode(val: Self) -> Self::Raw {
        ((val.releasable << 2) | (match val.mode {
            Unlocked => 0,
            Locked => 1,
            LockedDirty => 2,
        })) as usize
    }
    unsafe fn decode(val: Self::Raw) -> Self {
        ReleaseState {
            releasable: (val >> 2) as usize,
            mode: match val & 3 {
                0 => Unlocked,
                1 => Locked,
                2 => LockedDirty,
                _ => unreachable!()
            },
        }
    }
}

impl Packable for AcquireState {
    type Raw = usize;

    unsafe fn encode(val: Self) -> Self::Raw {
        match val {
            Queued(back) => back as usize,
            Available(available) => ((available << 1) | 1) as usize,
        }
    }
    unsafe fn decode(val: Self::Raw) -> Self {
        if val & 1 == 1 {
            Available((val >> 1) as usize)
        } else {
            Queued(val as *const Waiter)
        }
    }
}
