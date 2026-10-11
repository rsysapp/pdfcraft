//! Embed build: a per-thread bound on what decoding streams may produce, for a caller that bounds
//! its work (a save inspector; `hayro-interpret` sets it for each interpretation with a work
//! budget). Two quantities are bounded:
//! - bytes: every filter's output, each stopped as soon as it would pass what is left (a flate
//!   bomb is never allocated whole);
//! - codec work: an image codec's samples, weighed by the codec's cost per sample (DCT and CCITT
//!   one, JPEG 2000 four, JBIG2 four per pixel it makes or composes), checked before the codec
//!   runs where its header gives the size, and for JBIG2 counted while it runs.
//!
//! Past either, decoding fails and [`decode_budget_exceeded`] says so. Unbounded unless set.

/// Codec work per sample, by codec.
pub(crate) const DCT_WEIGHT: u64 = 1;
pub(crate) const CCITT_WEIGHT: u64 = 1;
pub(crate) const JPX_WEIGHT: u64 = 4;
pub(crate) const JBIG2_WEIGHT: u64 = 4;

#[cfg(feature = "std")]
mod imp {
    use core::cell::Cell;

    std::thread_local! {
        static BYTES_LEFT: Cell<u64> = const { Cell::new(u64::MAX) };
        static WORK_LEFT: Cell<u64> = const { Cell::new(u64::MAX) };
        static BYTES: Cell<u64> = const { Cell::new(0) };
        static WORK: Cell<u64> = const { Cell::new(0) };
        static EXCEEDED: Cell<bool> = const { Cell::new(false) };
    }

    /// Bound this thread's decoding from now on: `bytes` of filter output and `codec_work` of image
    /// codec work (`u64::MAX`: unbounded); what was spent is reset.
    pub fn set_decode_budget(bytes: u64, codec_work: u64) {
        BYTES_LEFT.with(|l| l.set(bytes));
        WORK_LEFT.with(|l| l.set(codec_work));
        BYTES.with(|b| b.set(0));
        WORK.with(|w| w.set(0));
        EXCEEDED.with(|e| e.set(false));
    }

    /// The bytes and codec work spent since the budget was set.
    pub fn decode_spent() -> (u64, u64) {
        (BYTES.with(Cell::get), WORK.with(Cell::get))
    }

    /// Whether a decode was refused for want of budget since it was set.
    pub fn decode_budget_exceeded() -> bool {
        EXCEEDED.with(Cell::get)
    }

    pub(crate) fn exceed() {
        EXCEEDED.with(|e| e.set(true));
    }

    fn take(left: &'static std::thread::LocalKey<Cell<u64>>, total: &'static std::thread::LocalKey<Cell<u64>>, amount: u64) -> bool {
        total.with(|t| t.set(t.get().saturating_add(amount)));
        let have = left.with(Cell::get);
        if have == u64::MAX {
            return true;
        }
        if amount > have {
            left.with(|l| l.set(0));
            exceed();
            return false;
        }
        left.with(|l| l.set(have - amount));
        true
    }

    pub(crate) fn bytes_room() -> usize {
        usize::try_from(BYTES_LEFT.with(Cell::get)).unwrap_or(usize::MAX)
    }

    pub(crate) fn spend_bytes(bytes: u64) -> bool {
        take(&BYTES_LEFT, &BYTES, bytes)
    }

    pub(crate) fn work_room() -> u64 {
        WORK_LEFT.with(Cell::get)
    }

    pub(crate) fn spend_work(work: u64) -> bool {
        take(&WORK_LEFT, &WORK, work)
    }
}

#[cfg(not(feature = "std"))]
mod imp {
    /// Bound this thread's decoding from now on: `bytes` of filter output and `codec_work` of image
    /// codec work (`u64::MAX`: unbounded); what was spent is reset.
    pub fn set_decode_budget(_bytes: u64, _codec_work: u64) {}

    /// The bytes and codec work spent since the budget was set.
    pub fn decode_spent() -> (u64, u64) {
        (0, 0)
    }

    /// Whether a decode was refused for want of budget since it was set.
    pub fn decode_budget_exceeded() -> bool {
        false
    }

    pub(crate) fn exceed() {}

    pub(crate) fn bytes_room() -> usize {
        usize::MAX
    }

    pub(crate) fn spend_bytes(_bytes: u64) -> bool {
        true
    }

    pub(crate) fn work_room() -> u64 {
        u64::MAX
    }

    pub(crate) fn spend_work(_work: u64) -> bool {
        true
    }
}

pub use imp::{decode_budget_exceeded, decode_spent, set_decode_budget};
pub(crate) use imp::{bytes_room, exceed, spend_bytes, spend_work, work_room};

/// Whether `bytes` more output fits what the byte budget leaves (always, without a budget).
pub(crate) fn bytes_fit(bytes: u64) -> bool {
    let room = bytes_room();
    room == usize::MAX || u64::try_from(room).map_or(true, |room| bytes <= room)
}

/// Whether a codec may make `samples` samples at `weight` work each: charges the work, or marks
/// the budget exceeded.
pub(crate) fn codec_allows(samples: u64, weight: u64) -> bool {
    spend_work(samples.saturating_mul(weight))
}
