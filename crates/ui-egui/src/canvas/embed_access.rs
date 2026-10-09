//! Embed build: what the host bridge (`crate::embed`) reads of a page view's private state.

use std::ops::Range;

use super::DocView;

impl DocView {
    /// The text selection as (page, glyph range), if any.
    pub(crate) fn selection_span(&self) -> Option<(usize, Range<usize>)> {
        self.selection.map(|s| (s.page, s.range()))
    }
}
