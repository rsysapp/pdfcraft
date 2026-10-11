//! Embed build: a per-thread log of what this parser resolved, for a caller that checks another
//! parser read the same file the same way (a save inspector). While a log is open, every indirect
//! object read through the cross-reference data is recorded with where it was read (an offset in
//! the file, or an object stream and index), and every stream read from the file with the range
//! of its data. Nothing is recorded unless a log is open.
//!
//! Also recorded: the decoded data of every content stream an interpreter reads besides a page's
//! own (form XObjects, tiling patterns' cells, Type 3 glyph procedures; see [`content`]), and
//! how many content streams were not read to their end (see [`content_stopped`]): an operator
//! after which the reader gives up, data it cannot parse, or a content stream it cannot decode.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Where an object was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// At this offset in the file (its `N G obj` header).
    InFile(usize),
    /// In the object stream with this number, at this index.
    InStream(u32, u32),
}

/// What was resolved while a log was open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Log {
    /// Object (number, generation) → where it was read. An object read from two places is in
    /// `conflicts`.
    pub objects: BTreeMap<(i32, i32), Place>,
    /// Stream object (number, generation) → its data's range in the file (start, length).
    pub streams: BTreeMap<(i32, i32), (usize, usize)>,
    /// Objects read from two different places, or streams read with two different data ranges
    /// (or decoded to two different contents, or a content stream without an object number).
    pub conflicts: usize,
    /// Content stream object (number, generation) → its data as decoded for interpretation.
    pub content: BTreeMap<(i32, i32), Vec<u8>>,
    /// Content streams whose reading stopped before their end, or that could not be decoded.
    pub stopped: usize,
}

#[cfg(feature = "std")]
mod imp {
    use super::{Log, Place};
    use core::cell::RefCell;

    std::thread_local! {
        static LOG: RefCell<Option<Log>> = const { RefCell::new(None) };
    }

    /// Open a log on this thread (an open one is emptied).
    pub fn open_log() {
        LOG.with(|log| *log.borrow_mut() = Some(Log::default()));
    }

    /// Close this thread's log and return it (`None`: none was open).
    pub fn close_log() -> Option<Log> {
        LOG.with(|log| log.borrow_mut().take())
    }

    pub(crate) fn object(id: (i32, i32), place: Place) {
        LOG.with(|log| {
            if let Some(log) = log.borrow_mut().as_mut()
                && *log.objects.entry(id).or_insert(place) != place
            {
                log.conflicts = log.conflicts.saturating_add(1);
            }
        });
    }

    pub(crate) fn stream(id: (i32, i32), range: (usize, usize)) {
        LOG.with(|log| {
            if let Some(log) = log.borrow_mut().as_mut()
                && *log.streams.entry(id).or_insert(range) != range
            {
                log.conflicts = log.conflicts.saturating_add(1);
            }
        });
    }

    /// A content stream (`id`: its object, `None` when it has none) is interpreted from `decoded`.
    pub fn content(id: Option<(i32, i32)>, decoded: &[u8]) {
        LOG.with(|log| {
            let mut log = log.borrow_mut();
            let Some(log) = log.as_mut() else { return };
            let same = match id {
                Some(id) => log.content.entry(id).or_insert_with(|| decoded.to_vec()).as_slice() == decoded,
                None => false,
            };
            if !same {
                log.conflicts = log.conflicts.saturating_add(1);
            }
        });
    }

    /// A content stream was not read to its end (or not at all: its data could not be decoded).
    pub fn content_stopped() {
        LOG.with(|log| {
            if let Some(log) = log.borrow_mut().as_mut() {
                log.stopped = log.stopped.saturating_add(1);
            }
        });
    }
}

#[cfg(not(feature = "std"))]
mod imp {
    use super::{Log, Place};

    /// Open a log on this thread (an open one is emptied).
    pub fn open_log() {}

    /// Close this thread's log and return it (`None`: none was open).
    pub fn close_log() -> Option<Log> {
        None
    }

    pub(crate) fn object(_: (i32, i32), _: Place) {}

    pub(crate) fn stream(_: (i32, i32), _: (usize, usize)) {}

    /// A content stream (`id`: its object, `None` when it has none) is interpreted from `decoded`.
    pub fn content(_: Option<(i32, i32)>, _: &[u8]) {}

    /// A content stream was not read to its end (or not at all: its data could not be decoded).
    pub fn content_stopped() {}
}

pub use imp::{close_log, content, content_stopped, open_log};
pub(crate) use imp::{object, stream};
