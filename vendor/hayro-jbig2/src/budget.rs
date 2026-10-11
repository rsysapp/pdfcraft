//! Embed build: a per-thread bound on the pixels decoding may touch, for a caller that bounds its
//! work (a save inspector). A bitmap's pixels are charged when it is made, a composition's when it
//! is done; past the allowance bitmaps are refused and compositions skipped, and
//! [`work_exceeded`] says so (the decode then fails). Unbounded unless set.

#[cfg(feature = "std")]
mod imp {
    use core::cell::Cell;

    std::thread_local! {
        static LEFT: Cell<u64> = const { Cell::new(u64::MAX) };
        static SPENT: Cell<u64> = const { Cell::new(0) };
        static EXCEEDED: Cell<bool> = const { Cell::new(false) };
    }

    pub fn set_work_allowance(pixels: u64) {
        LEFT.with(|l| l.set(pixels));
        SPENT.with(|s| s.set(0));
        EXCEEDED.with(|e| e.set(false));
    }

    pub fn work_spent() -> u64 {
        SPENT.with(Cell::get)
    }

    pub fn work_exceeded() -> bool {
        EXCEEDED.with(Cell::get)
    }

    pub(crate) fn spend(pixels: u64) -> bool {
        SPENT.with(|s| s.set(s.get().saturating_add(pixels)));
        let left = LEFT.with(Cell::get);
        if left == u64::MAX {
            return true;
        }
        if EXCEEDED.with(Cell::get) || pixels > left {
            EXCEEDED.with(|e| e.set(true));
            LEFT.with(|l| l.set(0));
            return false;
        }
        LEFT.with(|l| l.set(left - pixels));
        true
    }
}

#[cfg(not(feature = "std"))]
mod imp {
    pub fn set_work_allowance(_pixels: u64) {}

    pub fn work_spent() -> u64 {
        0
    }

    pub fn work_exceeded() -> bool {
        false
    }

    pub(crate) fn spend(_pixels: u64) -> bool {
        true
    }
}

pub use imp::{set_work_allowance, work_exceeded, work_spent};
pub(crate) use imp::spend;
