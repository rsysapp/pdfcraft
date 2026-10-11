use crate::CacheKey;
use crate::font::cid::Type0Font;
use crate::font::true_type::TrueTypeFont;
use crate::font::type1::Type1Font;
use hayro_cmap::BfString;
use kurbo::BezPath;
use skrifa::GlyphId;
use skrifa::outline::OutlinePen;
use std::rc::Rc;

/// Font data and metadata for downstream use.
#[derive(Clone)]
pub struct OutlineFontData {
    /// Raw font bytes (TrueType/OpenType/CFF data).
    pub data: crate::font::FontData,
    /// Cache key for font deduplication.
    pub cache_key: u128,
    /// PostScript name (e.g., "TimesNewRomanPS-BoldMT").
    pub postscript_name: Option<String>,
    /// Font weight (100-900, 400=normal, 700=bold).
    pub weight: Option<u32>,
    /// Whether the font is italic/oblique.
    pub is_italic: bool,
    /// Whether the font is serif (vs sans-serif).
    pub is_serif: bool,
    /// Whether the font is monospace.
    pub is_monospace: bool,
}

pub(crate) struct OutlinePath {
    path: BezPath,
    /// Embed build: the most elements kept (`MAX_OUTLINE_ELEMENTS` under a work budget), and
    /// whether the outline had more.
    limit: usize,
    overflowed: bool,
}

impl OutlinePath {
    pub(crate) fn new() -> Self {
        let limit = if crate::context::budgeted() { crate::context::MAX_OUTLINE_ELEMENTS } else { usize::MAX };
        Self { path: BezPath::new(), limit, overflowed: false }
    }

    /// Embed build: the outline, or an empty one when it had more elements than allowed (the
    /// work budget is then spent: the interpretation is too complex).
    pub(crate) fn take(self) -> BezPath {
        if self.overflowed {
            let _ = crate::context::spend_work(u64::MAX);
            return BezPath::new();
        }
        self.path
    }

    /// Embed build: room for one more element.
    #[inline]
    fn room(&mut self) -> bool {
        if self.path.elements().len() >= self.limit {
            self.overflowed = true;
            return false;
        }
        true
    }
}

impl OutlinePen for OutlinePath {
    #[inline]
    fn move_to(&mut self, x: f32, y: f32) {
        if self.room() {
            self.path.move_to((x, y));
        }
    }

    #[inline]
    fn line_to(&mut self, x: f32, y: f32) {
        if !self.path.elements().is_empty() && self.room() {
            self.path.line_to((x, y));
        }
    }

    #[inline]
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        if !self.path.elements().is_empty() && self.room() {
            self.path.quad_to((cx, cy), (x, y));
        }
    }

    #[inline]
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        if !self.path.elements().is_empty() && self.room() {
            self.path.curve_to((cx0, cy0), (cx1, cy1), (x, y));
        }
    }

    #[inline]
    fn close(&mut self) {
        if !self.path.elements().is_empty() && self.room() {
            self.path.close_path();
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum OutlineFont {
    Type1(Rc<Type1Font>),
    TrueType(Rc<TrueTypeFont>),
    Type0(Rc<Type0Font>),
}

impl CacheKey for OutlineFont {
    fn cache_key(&self) -> u128 {
        match self {
            Self::Type1(f) => f.cache_key(),
            Self::TrueType(t) => t.cache_key(),
            Self::Type0(t0) => t0.cache_key(),
        }
    }
}

impl OutlineFont {
    pub(crate) fn outline_glyph(&self, glyph: GlyphId, code: u32) -> BezPath {
        match self {
            Self::Type1(t) => t.outline_glyph(glyph),
            Self::TrueType(t) => t.outline_glyph(glyph),
            Self::Type0(t) => t.outline_glyph(glyph, code),
        }
    }

    pub(crate) fn char_code_to_unicode(&self, char_code: u32) -> Option<BfString> {
        match self {
            Self::Type1(t) => t.char_code_to_unicode(char_code),
            Self::TrueType(t) => t.char_code_to_unicode(char_code),
            Self::Type0(t) => t.char_code_to_unicode(char_code),
        }
    }

    /// Get the advance width for a glyph by character code.
    pub(crate) fn glyph_advance_width(&self, char_code: u32) -> Option<f32> {
        match self {
            Self::Type1(t) => t.glyph_width(char_code as u8),
            Self::TrueType(t) => Some(t.glyph_width(char_code as u8)),
            Self::Type0(t) => Some(t.code_advance(char_code).x as f32),
        }
    }

    /// Get raw font bytes and metadata.
    ///
    /// Returns None for Type1 fonts and non-embedded TrueType fonts.
    pub(crate) fn font_data(&self) -> Option<OutlineFontData> {
        match self {
            Self::Type1(_) => None,
            Self::TrueType(t) => Some(OutlineFontData {
                data: t.font_data()?,
                cache_key: t.cache_key(),
                postscript_name: t.postscript_name().map(|s| s.to_string()),
                weight: t.weight(),
                is_italic: t.is_italic(),
                is_serif: t.is_serif(),
                is_monospace: t.is_monospace(),
            }),
            Self::Type0(t) => Some(OutlineFontData {
                data: t.font_data()?,
                cache_key: t.cache_key(),
                postscript_name: t.postscript_name().map(|s| s.to_string()),
                weight: t.weight(),
                is_italic: t.is_italic(),
                is_serif: t.is_serif(),
                is_monospace: t.is_monospace(),
            }),
        }
    }
}
