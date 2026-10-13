//! Embed build: what the host bridge (`crate::embed`) reads of a page view's private state.

use std::ops::Range;

use super::{DocView, Selection, Span};

impl DocView {
    /// The text selection as (page, glyph range), if any. A column selection (upstream #740) is
    /// no run of glyphs in reading order, so it is not one the host can be told about.
    pub(crate) fn selection_span(&self) -> Option<(usize, Range<usize>)> {
        self.selection.and_then(|s| match s.span {
            Span::Run { anchor, head } => Some((s.page, Selection::range(anchor, head))),
            Span::Column { .. } => None,
        })
    }
}
