//! Embed build: what a save inspector reads through hayro: the page count, each page's annotation
//! count, the glyphs and images its content draws (page content only, see [`PageContent`]) and an
//! upright render of its content (no annotations, `/Rotate` ignored). Every optional-content
//! layer is drawn, whatever the document's configuration says: which layers show is a viewer's
//! choice, and an inspection must see all of the content. Each glyph, image and form drawn says
//! which optional content it is inside, as hayro met it (see [`PageContent::layers`]), for a
//! caller that evaluates it itself; [`ContentDocument::render_upright_hiding`] renders a page
//! with the content of chosen layers left out. Every answer is certain or an error: a
//! glyph that can be identified neither by its text nor by its font and glyph, an image that
//! cannot be identified, an interpreter warning (an unsupported font, an image that does not
//! decode) or a renderer panic fails the call instead of being skipped, and
//! [`ContentDocument::repaired`] says whether the parser had to rebuild the cross-reference data.
//!
//! Work is bounded without a clock: each interpretation (a page's text, a page's render) may do at
//! most the work budget given to [`ContentDocument::open`] (hayro's `work_budget`: operators,
//! glyphs, and paints weighed by the device area they cover); past it the call is
//! [`ContentError::TooComplex`]. Stream decoding (font programs, images, every stream the page
//! needs) is bounded per interpretation too (`hayro_syntax::budget`, set by the interpreter), and
//! running out of it is the same. [`ContentDocument::work_spent`] totals the work done, decoding
//! included, so a caller can bound a whole document too.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::pattern::Pattern;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, InterpreterWarning, Paint, PathDrawMode,
    SoftMask, TransformExt, current_layers, interpret_page, layers_report, work_spent,
};
use hayro::hayro_syntax::budget::{decode_budget_exceeded, decode_spent, set_decode_budget};
use hayro::hayro_syntax::object::{Array, Dict, Object, ObjectIdentifier};
use hayro::hayro_syntax::resolved;
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, render_upright};
use kurbo::{BezPath, Shape};

/// An optional-content mark content is drawn inside (see [`PageContent::layers`]).
pub use hayro::hayro_interpret::LayerMark;

use crate::raster::{RenderConfig, panic_message};

/// Why a file did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentOpenError {
    /// The file is encrypted (a password is needed, or its security is not supported).
    Encrypted,
    /// The file is not a PDF hayro can read.
    Invalid,
}

/// Why a page's text or render has no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentError {
    /// The page could not be read for certain (the message says why).
    Unreadable(String),
    /// The page needs more work than the budget allows.
    TooComplex,
}

impl From<String> for ContentError {
    fn from(message: String) -> Self {
        ContentError::Unreadable(message)
    }
}

/// The glyphs a page draws, in content order (no reading-order layout, no merging of glyphs drawn
/// twice): each glyph's text (its Unicode value: one character, several for a ligature, none
/// when the font maps it to an empty string; whitespace and control characters included, since a
/// drawn space is a drawn glyph) and its em box in the page's user space (PDF points): the
/// advance by the em square (descender −200 to ascender 800 of 1,000), as the font places it. A
/// glyph without a Unicode value has no characters, and is listed in `unnamed` by its font and
/// glyph instead.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageGlyphs {
    /// Every glyph's characters, one after the other.
    pub chars: Vec<char>,
    /// Per glyph: where its characters end in `chars`, and its box `[x0, y0, x1, y1]`.
    pub glyphs: Vec<(u32, [f32; 4])>,
    /// The glyphs without a Unicode value, in order: (index in `glyphs`, the font dictionary's
    /// object (number, generation), the glyph's ID in the font; for a Type 3 font, the code that
    /// selects its procedure).
    pub unnamed: Vec<(u32, (u32, u16), u32)>,
    /// The glyphs drawn inside optional content, in order: (index in `glyphs`, index in
    /// [`PageContent::layers`]).
    pub layers: Vec<(u32, u32)>,
    /// Embed build: every level of marked content (`BDC`, `BMC`) the page's own content and the
    /// forms it draws open, in the order they open: its tag and the level it opened inside
    /// (an index here; `u32::MAX`: none). Not a tiling pattern's cell's, a soft mask's group's or
    /// a Type 3 glyph procedure's (their glyphs are not text on the page).
    pub marked_levels: Vec<(Vec<u8>, u32)>,
    /// Embed build: the glyphs drawn inside marked content, in order: (index in `glyphs`, the
    /// innermost level open, an index in `marked_levels`).
    pub marked: Vec<(u32, u32)>,
}

impl PageGlyphs {
    /// Glyph `index`'s characters and box.
    pub fn glyph(&self, index: usize) -> Option<(&[char], [f32; 4])> {
        let (end, rect) = *self.glyphs.get(index)?;
        let start = match index.checked_sub(1) {
            Some(previous) => self.glyphs.get(previous)?.0,
            None => 0,
        };
        Some((self.chars.get(start as usize..end as usize)?, rect))
    }

    pub fn len(&self) -> usize {
        self.glyphs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

/// Where an image the interpreter drew comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageSource {
    /// An image XObject: its object number and generation, as hayro resolved it.
    Object { num: u32, generation: u16 },
    /// An inline image: its dictionary and its encoded data as hayro's content parser delimited
    /// them (the data ends at the `EI` hayro took for its end).
    Inline { dict: Arc<[u8]>, data: Arc<[u8]> },
}

/// One image hayro's interpreter drew: what the renderer is handed for it, in the same
/// interpretation that renders the page. The placement is hayro's (its `q`/`Q`, `cm`, form
/// `/Matrix`, …), not tracked beside it. What a clip hides of the image is not said: the whole
/// image is placed.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageDraw {
    pub source: ImageSource,
    /// Image space (the unit square, (0, 0) at the image's lower left) to user space (PDF
    /// points), `[a b c d e f]` as in a `cm`.
    pub matrix: [f64; 6],
    /// Drawn in a tiling pattern's cell, which repeats it over what the pattern paints: `matrix`
    /// is in the cell's pattern space, not the page's.
    pub tiled: bool,
    /// Drawn inside optional content: its index in [`PageContent::layers`].
    pub layer: Option<u32>,
}

/// One form XObject hayro's interpreter drew: which, and where its `/BBox` (the clip its content
/// is drawn under) lies.
#[derive(Clone, Debug, PartialEq)]
pub struct FormDraw {
    /// The form's object number and generation, as hayro resolved it.
    pub num: u32,
    pub generation: u16,
    /// The bounding box of the form's `/BBox` in user space (PDF points), `[x0, y0, x1, y1]`;
    /// for a `tiled` form, in the cell's pattern space.
    pub rect: [f64; 4],
    /// Drawn in a tiling pattern's cell.
    pub tiled: bool,
    /// Drawn inside optional content (its own `/OC` included): its index in
    /// [`PageContent::layers`].
    pub layer: Option<u32>,
}

/// What a page's content draws that a save inspector compares: its glyphs (the page's own
/// content) and every image drawn (in the page's content, in form XObjects, in soft masks'
/// groups, in tiling patterns' cells and in Type 3 glyph procedures), in drawing order; every
/// form XObject drawn; and what hayro read to draw them. Content hayro stops reading before its
/// end is an error (see [`ContentDocument::content`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageContent {
    pub glyphs: PageGlyphs,
    pub images: Vec<ImageDraw>,
    pub forms: Vec<FormDraw>,
    /// The page dictionary's object (`None`: hayro has no object number for it).
    pub page: Option<(u32, u16)>,
    /// The page's content streams as hayro decoded and joined them (`None`: hayro found none).
    pub decoded: Option<Vec<u8>>,
    /// Every object hayro read while interpreting the page.
    pub resolved: Resolved,
    /// The optional content glyphs, images and forms were drawn inside: each entry the marks
    /// open around one of them, outermost first (see `hayro_interpret::LayerMark`).
    pub layers: Vec<Vec<LayerMark>>,
    /// Every optional-content mark met while interpreting the page (around anything, paths and
    /// shadings included).
    pub layer_marks: Vec<LayerMark>,
    /// Marked content was unbalanced in a content stream (an `EMC` closing what the stream did
    /// not open, a level left open at its end): which content is in which layer is then each
    /// reader's guess.
    pub layers_unbalanced: bool,
}

/// Where hayro read an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectPlace {
    /// Its `N G obj` header at this offset in the file.
    InFile(u64),
    /// In an object stream, at an index.
    InStream { stream: u32, index: u32 },
}

/// The objects hayro read through the cross-reference data, each with where it read it, and the
/// file range of each stream's data as hayro delimited it: what another reader of the same file
/// must agree with for the two to describe one document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// (number, generation) and where.
    pub objects: Vec<((u32, u16), ObjectPlace)>,
    /// (number, generation) and the data's (start, length) in the file.
    pub streams: Vec<((u32, u16), (usize, usize))>,
    /// Objects hayro read from two places, or with an object number it cannot have read; content
    /// streams decoded two ways, or without an object number.
    pub conflicts: usize,
    /// (number, generation) and the decoded data of each content stream interpreted besides the
    /// page's own: form XObjects, tiling patterns' cells and Type 3 glyph procedures.
    pub content: Vec<((u32, u16), Vec<u8>)>,
    /// Content streams hayro did not read to their end (an operator after which it gives up,
    /// data it cannot parse) or could not decode.
    pub stopped: usize,
}

impl Resolved {
    fn from_log(log: Option<resolved::Log>) -> Self {
        let Some(log) = log else { return Self { conflicts: 1, ..Self::default() } };
        let mut out = Self { conflicts: log.conflicts, stopped: log.stopped, ..Self::default() };
        let id = |(num, generation): (i32, i32)| u32::try_from(num).ok().zip(u16::try_from(generation).ok());
        for (key, place) in log.objects {
            let place = match place {
                resolved::Place::InFile(offset) => u64::try_from(offset).ok().map(ObjectPlace::InFile),
                resolved::Place::InStream(stream, index) => Some(ObjectPlace::InStream { stream, index }),
            };
            match (id(key), place) {
                (Some(id), Some(place)) => out.objects.push((id, place)),
                _ => out.conflicts = out.conflicts.saturating_add(1),
            }
        }
        for (key, range) in log.streams {
            match id(key) {
                Some(id) => out.streams.push((id, range)),
                None => out.conflicts = out.conflicts.saturating_add(1),
            }
        }
        for (key, decoded) in log.content {
            match id(key) {
                Some(id) => out.content.push((id, decoded)),
                None => out.conflicts = out.conflicts.saturating_add(1),
            }
        }
        out
    }
}

/// Records, from hayro's interpretation, every glyph drawn with its text and its em box, and every
/// image drawn with its source and placement, in user space.
struct PageDevice<'a> {
    /// View space (where the interpreter draws) to user space.
    to_user: kurbo::Affine,
    /// Whether glyphs are recorded: the page's own content, not what a soft mask's group or a
    /// pattern's cell draws (those glyphs are not text on the page).
    with_glyphs: bool,
    /// Drawing a tiling pattern's cell.
    tiled: bool,
    found: PageGlyphs,
    /// Glyphs drawn that gave no text (no Unicode value) or no usable box.
    unread: usize,
    images: Vec<ImageDraw>,
    forms: Vec<FormDraw>,
    /// Images or forms drawn that could not be identified (an object number hayro cannot have
    /// read, or none).
    unidentified: usize,
    /// Soft masks already interpreted (see `SoftMask::use_key`), kept so their keys stay theirs.
    masks: Vec<SoftMask<'a>>,
    mask_keys: HashSet<usize>,
    /// Inline image bytes already copied, by where hayro holds them: one copy for the uses of an
    /// inline image in a form drawn many times (checked byte for byte: an address can be reused).
    inline: HashMap<(usize, usize), Arc<[u8]>>,
    /// The optional-content memberships met, and the last one looked up (by its `Arc`).
    layers: Vec<Arc<[LayerMark]>>,
    last_layer: Option<(Arc<[LayerMark]>, u32)>,
    /// Embed build: the levels of marked content open now (indices in `found.marked_levels`),
    /// innermost last.
    open_marked: Vec<u32>,
}

impl<'a> PageDevice<'a> {
    fn new(to_user: kurbo::Affine, with_glyphs: bool, tiled: bool) -> Self {
        Self {
            to_user,
            with_glyphs,
            tiled,
            found: PageGlyphs::default(),
            unread: 0,
            images: Vec::new(),
            forms: Vec::new(),
            unidentified: 0,
            masks: Vec::new(),
            mask_keys: HashSet::new(),
            inline: HashMap::new(),
            layers: Vec::new(),
            last_layer: None,
            open_marked: Vec::new(),
        }
    }

    /// The optional content what is drawn now is inside: its index in `layers` (`None`: none).
    fn layer(&mut self) -> Option<u32> {
        let current = current_layers()?;
        if let Some((last, index)) = &self.last_layer
            && Arc::ptr_eq(last, &current)
        {
            return Some(*index);
        }
        let index = match self.layers.iter().position(|known| **known == *current) {
            Some(index) => index,
            None => {
                self.layers.push(current.clone());
                self.layers.len().saturating_sub(1)
            }
        };
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        self.last_layer = Some((current, index));
        Some(index)
    }

    fn copied(&mut self, bytes: &[u8]) -> Arc<[u8]> {
        let key = (bytes.as_ptr() as usize, bytes.len());
        if let Some(found) = self.inline.get(&key)
            && **found == *bytes
        {
            return found.clone();
        }
        let copy: Arc<[u8]> = Arc::from(bytes);
        self.inline.insert(key, copy.clone());
        copy
    }

    /// Interpret what another device draws for this one (a soft mask's group, a pattern's cell)
    /// and keep the images it draws.
    fn nested(&mut self, to_user: kurbo::Affine, tiled: bool, draw: impl FnOnce(&mut PageDevice<'a>)) {
        let mut inner = PageDevice::new(to_user, false, tiled);
        inner.mask_keys = std::mem::take(&mut self.mask_keys);
        inner.masks = std::mem::take(&mut self.masks);
        inner.inline = std::mem::take(&mut self.inline);
        inner.layers = std::mem::take(&mut self.layers);
        draw(&mut inner);
        self.mask_keys = inner.mask_keys;
        self.masks = inner.masks;
        self.inline = inner.inline;
        self.layers = inner.layers;
        self.images.append(&mut inner.images);
        self.forms.append(&mut inner.forms);
        self.unidentified = self.unidentified.saturating_add(inner.unidentified);
    }

    /// The images a tiling pattern's cell draws, once per paint that uses it.
    fn paint(&mut self, paint: &Paint<'a>) {
        if let Paint::Pattern(pattern) = paint
            && let Pattern::Tiling(tiling) = &**pattern
        {
            // The cell's own space: its images are compared by identity, not placed on the page.
            self.nested(kurbo::Affine::IDENTITY, true, |inner| {
                let _ = tiling.interpret(inner, kurbo::Affine::IDENTITY, false);
            });
        }
    }

    /// The images a soft mask's group draws, once per setting of the mask.
    fn mask(&mut self, mask: Option<SoftMask<'a>>) {
        let Some(mask) = mask else { return };
        if !self.mask_keys.insert(mask.use_key()) {
            return;
        }
        let to_user = self.to_user;
        self.nested(to_user, self.tiled, |inner| mask.interpret(inner));
        self.masks.push(mask);
    }
}

impl<'a> Device<'a> for PageDevice<'a> {
    fn set_soft_mask(&mut self, mask: Option<SoftMask<'a>>) {
        self.mask(mask);
    }
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: kurbo::Affine, paint: &Paint<'a>, _: &PathDrawMode) {
        self.paint(paint);
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, mask: Option<SoftMask<'a>>, _: BlendMode) {
        self.mask(mask);
    }
    fn draw_glyph(&mut self, glyph: &Glyph<'a>, transform: kurbo::Affine, glyph_transform: kurbo::Affine, paint: &Paint<'a>, _: &GlyphDrawMode) {
        self.paint(paint);
        // A Type 3 glyph's procedure is content like any other: what it draws (images, forms) and
        // how far it is read count. Its own glyphs are not text on the page.
        if let Glyph::Type3(procedure) = glyph {
            let to_user = self.to_user;
            self.nested(to_user, self.tiled, |inner| procedure.interpret(inner, transform, glyph_transform, paint));
        }
        if !self.with_glyphs {
            return;
        }
        // A glyph without a Unicode value is identified by its font and glyph instead (a font
        // with no object cannot be named: the glyph is not read).
        let (text, unnamed) = match glyph.as_unicode() {
            Some(hayro::hayro_interpret::hayro_cmap::BfString::Char(c)) => (c.to_string(), None),
            Some(hayro::hayro_interpret::hayro_cmap::BfString::String(s)) => (s, None),
            None => match glyph
                .identity()
                .and_then(|(font, id)| u32::try_from(font.obj_number).ok().zip(u16::try_from(font.gen_number).ok()).map(|f| (f, id)))
            {
                Some(identity) => (String::new(), Some(identity)),
                None => {
                    self.unread = self.unread.saturating_add(1);
                    return;
                }
            },
        };
        // Glyph space uses 1,000 units per em; the em box spans the advance by −200…800, as the
        // text extraction places glyphs (`text::TextDevice`). Without an advance, the outline's
        // width (at least half an em) stands in.
        let advance = match glyph {
            Glyph::Outline(g) => g
                .advance_width()
                .filter(|a| a.is_finite() && *a > 0.0)
                .map(f64::from)
                .unwrap_or_else(|| g.outline().bounding_box().width().max(500.0)),
            Glyph::Type3(g) => g.advance_width().filter(|a| a.is_finite() && *a > 0.0).map(f64::from).unwrap_or(600.0),
        };
        let em = kurbo::Rect::new(0.0, -200.0, advance, 800.0);
        let b = (self.to_user * transform * glyph_transform * em.to_path(0.1)).bounding_box();
        if !(b.x0.is_finite() && b.y0.is_finite() && b.x1.is_finite() && b.y1.is_finite()) || b.width() > 10_000.0 || b.height() > 10_000.0 {
            self.unread = self.unread.saturating_add(1);
            return;
        }
        let index = u32::try_from(self.found.glyphs.len()).unwrap_or(u32::MAX);
        if let Some((font, id)) = unnamed {
            self.found.unnamed.push((index, font, id));
        }
        if let Some(layer) = self.layer() {
            self.found.layers.push((index, layer));
        }
        if let Some(level) = self.open_marked.last() {
            self.found.marked.push((index, *level));
        }
        self.found.chars.extend(text.chars());
        let end = u32::try_from(self.found.chars.len()).unwrap_or(u32::MAX);
        self.found.glyphs.push((end, [b.x0 as f32, b.y0 as f32, b.x1 as f32, b.y1 as f32]));
    }
    fn draw_image(&mut self, image: Image<'a, '_>, transform: kurbo::Affine) {
        let (width, height) = (f64::from(image.width()), f64::from(image.height()));
        let stream = match &image {
            Image::Raster(raster) => raster.stream().clone(),
            Image::Stencil(stencil) => {
                self.paint(stencil.paint());
                stencil.stream().clone()
            }
        };
        let source = match stream.dict().obj_id() {
            Some(id) => match (u32::try_from(id.obj_number), u16::try_from(id.gen_number)) {
                (Ok(num), Ok(generation)) => ImageSource::Object { num, generation },
                _ => {
                    self.unidentified = self.unidentified.saturating_add(1);
                    return;
                }
            },
            None => ImageSource::Inline { dict: self.copied(stream.dict().data()), data: self.copied(&stream.raw_data()) },
        };
        // hayro hands the device the transformation of the image's pixel grid (row 0 at the top):
        // the unit square's is that after (u, v) ↦ (u · width, (1 − v) · height).
        let unit = self.to_user * transform * kurbo::Affine::new([width, 0.0, 0.0, -height, 0.0, height]);
        let layer = self.layer();
        self.images.push(ImageDraw { source, matrix: unit.as_coeffs(), tiled: self.tiled, layer });
    }
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    // Embed build: which level of marked content each glyph is drawn in (the save inspector's
    // R6b-47). Only the page's own content and its forms (`with_glyphs`).
    fn begin_marked_content(&mut self, tag: &[u8], _mcid: Option<i32>) {
        if !self.with_glyphs {
            return;
        }
        let parent = self.open_marked.last().copied().unwrap_or(u32::MAX);
        let index = u32::try_from(self.found.marked_levels.len()).unwrap_or(u32::MAX);
        self.found.marked_levels.push((tag.to_vec(), parent));
        self.open_marked.push(index);
    }
    fn end_marked_content(&mut self) {
        if self.with_glyphs {
            self.open_marked.pop();
        }
    }
    fn draw_form(&mut self, id: Option<ObjectIdentifier>, bbox: &BezPath) {
        let Some((num, generation)) = id.and_then(|id| u32::try_from(id.obj_number).ok().zip(u16::try_from(id.gen_number).ok())) else {
            self.unidentified = self.unidentified.saturating_add(1);
            return;
        };
        let b = (self.to_user * bbox.clone()).bounding_box();
        let layer = self.layer();
        self.forms.push(FormDraw { num, generation, rect: [b.x0, b.y0, b.x1, b.y1], tiled: self.tiled, layer });
    }
}

/// The deepest nesting of arrays and dictionaries [`object_shape`] writes out; deeper, a marker.
pub const SHAPE_DEPTH: usize = 256;

/// Stream-dictionary entries a shape leaves out: how the data is delimited and coded, which a
/// caller compares on its own (the data's extent, the coding as each reader applies it).
pub const SHAPE_LEFT_OUT: [&[u8]; 3] = [b"Length", b"Filter", b"DecodeParms"];

/// The canonical form of an object, written the same way for any reader's parse of it:
/// `n` null; `b` and a byte for a boolean; `#` and the 8 bytes of the number as a 64-bit float
/// (an integer as its value; -0 as 0); `s` or `/`, the length (8 bytes) and the bytes of a
/// string or a name (escapes resolved); `[` and the count, then the items; `<` and the count,
/// then each entry (key length, key, value) in key order; `R`, the number and the generation
/// (4 and 2 bytes) of a reference; `S` and its dictionary, less [`SHAPE_LEFT_OUT`], for a
/// stream; `!` past [`SHAPE_DEPTH`].
pub fn object_shape(kind: ShapeItem<'_>, out: &mut Vec<u8>) {
    match kind {
        ShapeItem::Null => out.push(b'n'),
        ShapeItem::Bool(value) => out.extend_from_slice(&[b'b', u8::from(value)]),
        ShapeItem::Number(value) => {
            out.push(b'#');
            let value = if value == 0.0 { 0.0 } else { value };
            out.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        ShapeItem::String(bytes) | ShapeItem::Name(bytes) => {
            out.push(if matches!(kind, ShapeItem::String(_)) { b's' } else { b'/' });
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        ShapeItem::Reference(num, generation) => {
            out.push(b'R');
            out.extend_from_slice(&num.to_le_bytes());
            out.extend_from_slice(&generation.to_le_bytes());
        }
    }
}

/// One scalar item of a shape (see [`object_shape`]).
#[derive(Clone, Copy, Debug)]
pub enum ShapeItem<'a> {
    Null,
    Bool(bool),
    Number(f64),
    String(&'a [u8]),
    Name(&'a [u8]),
    Reference(u32, u16),
}

/// The shape of a reference as hayro holds it: number and generation, or an impossible pair.
fn shape_reference(reference: hayro::hayro_syntax::object::ObjRef, out: &mut Vec<u8>) {
    match (u32::try_from(reference.obj_number), u16::try_from(reference.gen_number)) {
        (Ok(num), Ok(generation)) => object_shape(ShapeItem::Reference(num, generation), out),
        _ => out.push(b'?'),
    }
}

fn shape_of(object: &Object<'_>, depth: usize, out: &mut Vec<u8>) {
    use hayro::hayro_syntax::object::MaybeRef;
    if depth > SHAPE_DEPTH {
        out.push(b'!');
        return;
    }
    let item = |value: &MaybeRef<Object<'_>>, out: &mut Vec<u8>| match value {
        MaybeRef::Ref(reference) => shape_reference(*reference, out),
        MaybeRef::NotRef(object) => shape_of(object, depth + 1, out),
    };
    let dict = |dict: &Dict<'_>, left_out: &[&[u8]], out: &mut Vec<u8>| {
        let entries: Vec<_> = dict.entries().filter(|(key, _)| !left_out.contains(&key.as_ref())).collect();
        out.push(b'<');
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (key, value) in &entries {
            out.extend_from_slice(&(key.as_ref().len() as u64).to_le_bytes());
            out.extend_from_slice(key.as_ref());
            item(value, out);
        }
    };
    match object {
        Object::Null(_) => object_shape(ShapeItem::Null, out),
        Object::Boolean(value) => object_shape(ShapeItem::Bool(*value), out),
        Object::Number(number) => object_shape(ShapeItem::Number(number.as_f64()), out),
        Object::String(string) => object_shape(ShapeItem::String(string.as_bytes()), out),
        Object::Name(name) => object_shape(ShapeItem::Name(name.as_ref()), out),
        Object::Array(array) => {
            let items: Vec<_> = array.raw_iter().collect();
            out.push(b'[');
            out.extend_from_slice(&(items.len() as u64).to_le_bytes());
            for value in &items {
                item(value, out);
            }
        }
        Object::Dict(found) => dict(found, &[], out),
        Object::Stream(stream) => {
            out.push(b'S');
            dict(stream.dict(), &SHAPE_LEFT_OUT, out);
        }
    }
}

/// A page's content rendered upright: `rgba` is premultiplied RGBA8, `width` × `height` pixels.
/// User space maps to pixels as `px = (x − crop[0]) · scale`, `py = (height_pt − (y − crop[1])) · scale`.
#[derive(Clone, Debug)]
pub struct UprightRaster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub scale: f32,
    /// The intersected crop box (x0, y0, x1, y1), user space.
    pub crop: [f32; 4],
    /// The page's height in points (upright).
    pub height_pt: f32,
}

/// One document opened by hayro for inspection.
pub struct ContentDocument {
    bytes: Arc<Vec<u8>>,
    pdf: Pdf,
    settings: InterpreterSettings,
    warnings: Arc<AtomicUsize>,
    /// Interpretations that ran out of their work budget.
    exhausted: Arc<AtomicUsize>,
    spent: AtomicU64,
    /// What hayro read opening the file and its page tree.
    opened: Resolved,
}

impl ContentDocument {
    /// Open `bytes` with the empty password. An encrypted file is [`ContentOpenError::Encrypted`]
    /// whether or not the empty password opens it: hayro's typed decryption errors say which.
    /// `work_budget` bounds each interpretation (see the module documentation).
    pub fn open(bytes: Arc<Vec<u8>>, work_budget: u64) -> Result<Self, ContentOpenError> {
        resolved::open_log();
        let opened = catch_unwind(AssertUnwindSafe(|| {
            let pdf = Pdf::new_with_password(bytes.clone(), "");
            if let Ok(pdf) = &pdf {
                // The page tree is read here, inside the log.
                let _ = pdf.pages().len();
            }
            pdf
        }));
        let read_at_open = Resolved::from_log(resolved::close_log());
        let pdf = match opened {
            Ok(Ok(pdf)) => pdf,
            Ok(Err(LoadPdfError::Decryption(_))) => return Err(ContentOpenError::Encrypted),
            Ok(Err(LoadPdfError::Invalid)) | Err(_) => return Err(ContentOpenError::Invalid),
        };
        let warnings = Arc::new(AtomicUsize::new(0));
        let exhausted = Arc::new(AtomicUsize::new(0));
        let (counter, budget_counter) = (warnings.clone(), exhausted.clone());
        let settings = InterpreterSettings {
            // Page content only: annotations are compared object by object, not drawn.
            render_annotations: false,
            // Every layer: content a viewer hides by default is still content.
            all_layers_visible: true,
            // Text that only clips is text too: other readers extract it.
            report_clip_glyphs: true,
            work_budget,
            warning_sink: Arc::new(move |warning| {
                let counter = if matches!(warning, InterpreterWarning::WorkBudgetExceeded) { &budget_counter } else { &counter };
                counter.fetch_add(1, Ordering::Relaxed);
            }),
            ..RenderConfig::default().settings()
        };
        Ok(Self { bytes, pdf, settings, warnings, exhausted, spent: AtomicU64::new(0), opened: read_at_open })
    }

    /// What hayro read opening the file: the catalog, the page tree and the pages (see
    /// [`Resolved`]).
    pub fn opened(&self) -> &Resolved {
        &self.opened
    }

    /// Read each of `objects` (number, generation) through hayro's cross-reference data, as it
    /// reads an object a reference names, logging where it found each (see [`Resolved`]): for a
    /// caller that checks every object another reader read is one hayro reads at the same place,
    /// with the same data. Also returns the objects hayro does not read at all.
    pub fn read_objects(&self, objects: &[(u32, u16)]) -> (Resolved, Vec<(u32, u16)>) {
        resolved::open_log();
        let unread = catch_unwind(AssertUnwindSafe(|| {
            let xref = self.pdf.xref();
            objects
                .iter()
                .copied()
                .filter(|(num, generation)| match i32::try_from(*num) {
                    Ok(num) => xref.get::<Object<'_>>(ObjectIdentifier::new(num, i32::from(*generation))).is_none(),
                    Err(_) => true,
                })
                .collect::<Vec<_>>()
        }));
        let read = Resolved::from_log(resolved::close_log());
        match unread {
            Ok(unread) => (read, unread),
            // A panic reads nothing for certain.
            Err(_) => (Resolved { conflicts: 1, ..read }, objects.to_vec()),
        }
    }

    /// Each of `objects` (number, generation) as hayro parses it, in a canonical form another
    /// reader can compare with its own parse (see [`object_shape`]); `None` where hayro reads
    /// nothing. With [`ContentDocument::read_objects`] (where each object is, and its stream
    /// data), a caller checks that no value it reads from a dictionary or array is one the two
    /// readers parse differently.
    pub fn object_shapes(&self, objects: &[(u32, u16)]) -> Vec<Option<Vec<u8>>> {
        let shapes = catch_unwind(AssertUnwindSafe(|| {
            let xref = self.pdf.xref();
            objects
                .iter()
                .map(|(num, generation)| {
                    let num = i32::try_from(*num).ok()?;
                    let object = xref.get::<Object<'_>>(ObjectIdentifier::new(num, i32::from(*generation)))?;
                    let mut out = Vec::new();
                    shape_of(&object, 0, &mut out);
                    Some(out)
                })
                .collect::<Vec<_>>()
        }));
        // A panic reads nothing for certain.
        shapes.unwrap_or_else(|_| vec![None; objects.len()])
    }

    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    /// How many entries of the page's `/Annots` are dictionaries (`None`: no such page).
    pub fn annotation_count(&self, page: usize) -> Option<usize> {
        let pages = self.pdf.pages();
        let page = pages.get(page)?;
        Some(page.raw().get::<Array<'_>>(b"Annots").map_or(0, |annots| annots.iter::<Dict<'_>>().count()))
    }

    /// The parser rebuilt the cross-reference data (on opening, or when an object was not where
    /// the table said): what it reads may not be what another reader of the file reads.
    pub fn repaired(&self) -> bool {
        self.pdf.xref().is_repaired()
    }

    fn warnings(&self) -> usize {
        self.warnings.load(Ordering::Relaxed)
    }

    /// The work every interpretation so far has done, in hayro's work units.
    pub fn work_spent(&self) -> u64 {
        self.spent.load(Ordering::Relaxed)
    }

    /// After an interpretation: count its work, and refuse it when it ran out of budget or the
    /// interpreter reported a problem. Its stream decoding counts too (one unit per 256 bytes
    /// decoded and per 64 units of image codec work, `hayro_syntax::budget`), and running out of
    /// decoding budget is running out of budget.
    fn finish(&self, number: usize, (warnings, exhausted): (usize, usize)) -> Result<(), ContentError> {
        let (bytes, codec) = decode_spent();
        let decode_exceeded = decode_budget_exceeded();
        set_decode_budget(u64::MAX, u64::MAX);
        self.spent.fetch_add(work_spent().saturating_add(bytes / 256).saturating_add(codec / 64), Ordering::Relaxed);
        if self.exhausted.load(Ordering::Relaxed) != exhausted || decode_exceeded {
            return Err(ContentError::TooComplex);
        }
        if self.warnings() != warnings {
            return Err(ContentError::Unreadable(format!("page {number}: the interpreter reported a problem")));
        }
        Ok(())
    }

    fn counts(&self) -> (usize, usize) {
        (self.warnings(), self.exhausted.load(Ordering::Relaxed))
    }

    /// The glyphs the page draws (see [`PageGlyphs`]); [`ContentDocument::content`] without the
    /// images.
    pub fn text(&self, page: usize) -> Result<PageGlyphs, ContentError> {
        self.content(page).map(|content| content.glyphs)
    }

    /// The glyphs and images the page draws (see [`PageContent`]), from one interpretation.
    /// Fails when a drawn glyph gives neither text (a Unicode mapping) nor a font object to name it
    /// by, or no usable box, when a drawn
    /// image or form cannot be identified, when hayro stops reading a content stream before its
    /// end (an operator after which it gives up, data it cannot parse) or cannot decode one, on an
    /// interpreter warning, on a panic, when the parser had to rebuild the cross-reference data,
    /// and past the work budget.
    pub fn content(&self, page: usize) -> Result<PageContent, ContentError> {
        let before = self.counts();
        let number = page.saturating_add(1);
        let extracted = catch_unwind(AssertUnwindSafe(|| {
            let pages = self.pdf.pages();
            let p = pages.get(page)?;
            let initial = p.initial_transform(true).to_kurbo();
            if initial.determinant().abs() <= f64::EPSILON {
                return Some(Err(()));
            }
            let (w, h) = p.render_dimensions();
            let cache = InterpreterCache::new();
            let mut ctx = Context::new(initial, kurbo::Rect::new(0.0, 0.0, f64::from(w), f64::from(h)), &cache, p.xref(), self.settings.clone());
            let mut device = PageDevice::new(initial.inverse(), true, false);
            resolved::open_log();
            let decoded = p.page_stream().map(<[u8]>::to_vec);
            interpret_page(p, &mut ctx, &mut device);
            let read = Resolved::from_log(resolved::close_log());
            // What the page's content left open is closed (and counted unbalanced) with it.
            drop(ctx);
            let (layer_marks, layers_unbalanced) = layers_report();
            let page = p.raw().obj_id().and_then(|id| u32::try_from(id.obj_number).ok().zip(u16::try_from(id.gen_number).ok()));
            let layers = device.layers.iter().map(|layer| layer.to_vec()).collect();
            let content = PageContent {
                glyphs: device.found,
                images: device.images,
                forms: device.forms,
                page,
                decoded,
                resolved: read,
                layers,
                layer_marks,
                layers_unbalanced,
            };
            Some(Ok((content, device.unread, device.unidentified)))
        }));
        // A panic above leaves the log open: close it.
        let _ = resolved::close_log();
        let (content, unread, unidentified) = match extracted {
            Ok(Some(Ok(found))) => found,
            Ok(Some(Err(()))) => return Err(format!("page {number} has no usable page box").into()),
            Ok(None) => return Err(format!("page {number} does not exist").into()),
            Err(panic) => return Err(format!("text extraction crashed on page {number}: {}", panic_message(&panic)).into()),
        };
        self.finish(number, before)?;
        if unread > 0 {
            return Err(format!("page {number}: {unread} glyph(s) have neither text nor a font object to name them by, or no usable box").into());
        }
        if unidentified > 0 {
            return Err(format!("page {number}: {unidentified} image(s) or form(s) cannot be identified").into());
        }
        if content.resolved.stopped > 0 {
            return Err(
                format!("page {number}: the interpreter stopped reading {} content stream(s) before their end", content.resolved.stopped).into()
            );
        }
        if self.repaired() {
            return Err(ContentError::Unreadable("the file's cross-reference data is damaged".into()));
        }
        Ok(content)
    }

    /// The page's media box and crop box (`[x0, y0, x1, y1]`, normalised) and its rotation in
    /// degrees (0, 90, 180 or 270), as hayro took them, inherited through the page tree: for a
    /// caller that checks another reader inherits the same (hayro skips a value it cannot read
    /// and takes the inherited one, or a default).
    pub fn page_geometry(&self, page: usize) -> Option<([f64; 4], [f64; 4], u16)> {
        let pages = self.pdf.pages();
        let page = pages.get(page)?;
        let (media, crop) = (page.media_box(), page.crop_box());
        let rotation = match page.rotation() {
            hayro::hayro_syntax::page::Rotation::None => 0,
            hayro::hayro_syntax::page::Rotation::Horizontal => 90,
            hayro::hayro_syntax::page::Rotation::Flipped => 180,
            hayro::hayro_syntax::page::Rotation::FlippedHorizontal => 270,
        };
        Some(([media.x0, media.y0, media.x1, media.y1], [crop.x0, crop.y0, crop.x1, crop.y1], rotation))
    }

    /// The page box hayro renders upright (intersected crop box), in points: width, height.
    pub fn upright_size(&self, page: usize) -> Option<(f32, f32)> {
        self.pdf.pages().get(page).map(|page| page.base_dimensions())
    }

    /// Render the page's content upright, at the scale that makes its longer side `long_side`
    /// pixels. Fails on an interpreter warning, on a panic, when the parser has to rebuild the
    /// cross-reference data meanwhile, and past the work budget.
    ///
    /// Each render reads the file afresh: hayro keeps what it decoded for a page as long as its
    /// document lives (tens of megabytes for a page of heavy content), so rendering every page of
    /// one document would hold all of it at once.
    pub fn render_upright(&self, page: usize, long_side: u32) -> Result<UprightRaster, ContentError> {
        self.render_upright_hiding(page, long_side, &[])
    }

    /// [`ContentDocument::render_upright`], with the content inside the optional-content groups
    /// and membership dictionaries `hidden` (number, generation) left out: the page as a viewer
    /// shows it by default, where the caller evaluated which those are.
    pub fn render_upright_hiding(&self, page: usize, long_side: u32, hidden: &[(i32, i32)]) -> Result<UprightRaster, ContentError> {
        let settings = InterpreterSettings { hidden_layers: Arc::new(hidden.to_vec()), ..self.settings.clone() };
        let before = self.counts();
        let number = page.saturating_add(1);
        // A file is parsed afresh without a decoding budget (an interpretation sets its own).
        set_decode_budget(u64::MAX, u64::MAX);
        let pdf = match catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(self.bytes.clone(), ""))) {
            Ok(Ok(pdf)) => pdf,
            _ => return Err(ContentError::Unreadable("the file could not be parsed again for rendering".into())),
        };
        let rendered = catch_unwind(AssertUnwindSafe(|| {
            let pages = pdf.pages();
            let page = pages.get(page).ok_or_else(|| format!("page {number} does not exist"))?;
            let (width_pt, height_pt) = page.base_dimensions();
            if !(width_pt.is_finite() && height_pt.is_finite()) || width_pt < 0.5 || height_pt < 0.5 {
                return Err(format!("page {number} has an empty or invalid page box"));
            }
            let side = long_side.clamp(1, u32::from(u16::MAX)) as f32;
            let scale = side / width_pt.max(height_pt);
            let pixels = |pt: f32| crate::raster::device_pixels(pt, scale).min(side as u32);
            let (width, height) = (pixels(width_pt), pixels(height_pt));
            let interpreter = &settings;
            let settings = RenderSettings {
                x_scale: scale,
                y_scale: scale,
                width: u16::try_from(width).ok(),
                height: u16::try_from(height).ok(),
                bg_color: WHITE,
                ..Default::default()
            };
            let cache = RenderCache::new();
            let pixmap = render_upright(page, &cache, interpreter, &settings);
            let crop = page.intersected_crop_box();
            Ok(UprightRaster {
                width: u32::from(pixmap.width()),
                height: u32::from(pixmap.height()),
                rgba: pixmap.data_as_u8_slice().to_vec(),
                scale,
                crop: [crop.x0 as f32, crop.y0 as f32, crop.x1 as f32, crop.y1 as f32],
                height_pt,
            })
        }));
        let raster = match rendered {
            Ok(result) => result?,
            Err(panic) => return Err(format!("renderer crashed on page {number}: {}", panic_message(&panic)).into()),
        };
        self.finish(number, before)?;
        if pdf.xref().is_repaired() {
            return Err(ContentError::Unreadable("the file's cross-reference data is damaged".into()));
        }
        Ok(raster)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const PAGE: &str = "<< /Type /Page /Parent 2 0 R >>";

    /// A one-page 200 × 300 file; `startxref_shift` moves `startxref` off the table.
    fn file(page: &str, startxref_shift: usize) -> Vec<u8> {
        let bodies = ["<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>", page];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref + startxref_shift).as_bytes());
        out
    }

    #[test]
    fn says_when_the_cross_reference_data_was_rebuilt() {
        let sound = ContentDocument::open(Arc::new(file(PAGE, 0)), u64::MAX).unwrap();
        assert!(!sound.repaired());
        assert_eq!(sound.page_count(), 1);
        // `startxref` points into nothing: hayro rebuilds the table by scanning.
        let damaged = ContentDocument::open(Arc::new(file(PAGE, 3)), u64::MAX).unwrap();
        assert!(damaged.repaired());
    }

    #[test]
    fn stops_an_interpretation_at_its_work_budget() {
        // 1,000 small fills: about 5,000 units of work (2 operators and a 3-unit paint each).
        let fills = "1 1 1 1 re f\n".repeat(1_000);
        let with_content = |budget| {
            let mut out = b"%PDF-1.7\n".to_vec();
            let bodies = [
                "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_string(),
                "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_string(),
                format!("<< /Length {} >>\nstream\n{fills}\nendstream", fills.len()),
            ];
            let mut offsets = Vec::new();
            for (i, body) in bodies.iter().enumerate() {
                offsets.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
            }
            let xref = out.len();
            out.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
            for offset in offsets {
                out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
            ContentDocument::open(Arc::new(out), budget).unwrap()
        };
        let ample = with_content(1_000_000);
        assert!(ample.text(0).is_ok());
        assert!(ample.render_upright(0, 300).is_ok());
        assert!(ample.work_spent() >= 2 * 5_000, "{}", ample.work_spent());
        let tight = with_content(1_000);
        assert_eq!(tight.text(0), Err(ContentError::TooComplex));
        assert!(matches!(tight.render_upright(0, 300), Err(ContentError::TooComplex)));
    }

    #[test]
    fn renders_a_rotated_page_upright() {
        let doc = ContentDocument::open(Arc::new(file("<< /Type /Page /Parent 2 0 R /Rotate 90 >>", 0)), u64::MAX).unwrap();
        assert!(!doc.repaired());
        let raster = doc.render_upright(0, 300).unwrap();
        // Upright: 200 × 300 points, longer side 300 pixels, whatever `/Rotate` says.
        assert_eq!((raster.width, raster.height), (200, 300));
    }

    /// A one-page lopdf document: `content` drawn with `resources`; also returns the object id
    /// of `extra` (a stream added to the file) when given.
    fn lopdf_page(content: &[u8], resources: lopdf::Dictionary, extra: Option<lopdf::Stream>) -> (Vec<u8>, Option<(u32, u16)>) {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.7");
        let extra = extra.map(|stream| doc.add_object(stream));
        let content = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content, "Resources" => resources,
            "MediaBox" => vec![Object::Integer(0), Object::Integer(0), Object::Integer(200), Object::Integer(300)],
        });
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        (bytes, extra)
    }

    #[test]
    fn decoding_stops_at_the_decode_budget() {
        use hayro::hayro_syntax::object::{ObjectIdentifier, Stream};
        let data = b"1 0 0 rg 10 10 20 20 re f\n".repeat(2_000);
        let mut stream = lopdf::Stream::new(lopdf::dictionary! {}, data.clone());
        stream.compress().unwrap();
        let (bytes, id) = lopdf_page(b"", lopdf::dictionary! {}, Some(stream));
        let (num, generation) = id.unwrap();
        let pdf = Pdf::new(Arc::new(bytes)).unwrap();
        let read = || pdf.xref().get::<Stream<'_>>(ObjectIdentifier::new(num as i32, i32::from(generation))).unwrap().decoded().map(|d| d.len());
        // Without a budget, as before.
        set_decode_budget(u64::MAX, u64::MAX);
        assert_eq!(read().ok(), Some(data.len()));
        assert!(!decode_budget_exceeded());
        // Under one, decoding stops at it and says so.
        set_decode_budget(1_024, u64::MAX);
        assert!(read().is_err());
        assert!(decode_budget_exceeded());
        set_decode_budget(u64::MAX, u64::MAX);
        assert_eq!(read().ok(), Some(data.len()));
    }

    /// A page showing "Hello" in an embedded TrueType font (Inter).
    fn embedded_font_page() -> Vec<u8> {
        use lopdf::{Object, Stream, dictionary};
        let program = include_bytes!("../../../assets/fonts/Inter-Regular.ttf").to_vec();
        let mut font_file = Stream::new(dictionary! { "Length1" => program.len() as i64 }, program);
        font_file.compress().unwrap();
        let mut doc_bytes = Vec::new();
        let mut doc = lopdf::Document::with_version("1.7");
        let file = doc.add_object(font_file);
        let descriptor = doc.add_object(dictionary! {
            "Type" => "FontDescriptor", "FontName" => "Inter", "Flags" => 32, "ItalicAngle" => 0, "Ascent" => 900, "Descent" => -200,
            "CapHeight" => 700, "StemV" => 80, "FontBBox" => vec![Object::Integer(0), Object::Integer(-200), Object::Integer(1000), Object::Integer(900)],
            "FontFile2" => file,
        });
        let widths: Vec<Object> = (32..127).map(|_| Object::Integer(600)).collect();
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Inter", "FirstChar" => 32, "LastChar" => 126, "Widths" => widths,
            "Encoding" => "WinAnsiEncoding", "FontDescriptor" => descriptor,
        });
        let content = doc.add_object(Stream::new(dictionary! {}, b"BT /F1 24 Tf 20 150 Td (Hello) Tj ET".to_vec()));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content, "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "MediaBox" => vec![Object::Integer(0), Object::Integer(0), Object::Integer(200), Object::Integer(300)],
        });
        doc.objects.insert(pages, dictionary! {"Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1}.into());
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        doc.save_to(&mut doc_bytes).unwrap();
        doc_bytes
    }

    #[test]
    fn an_embedded_font_program_and_its_outlines_are_work() {
        let file = Arc::new(embedded_font_page());
        // An ordinary font reads and renders under the inspector's budget, and costs work.
        let ample = ContentDocument::open(file.clone(), 1_000_000).unwrap();
        assert_eq!(ample.text(0).unwrap().chars, "Hello".chars().collect::<Vec<_>>());
        let before = ample.work_spent();
        assert!(ample.render_upright(0, 300).is_ok());
        assert!(ample.work_spent() - before > 300, "{}", ample.work_spent() - before);
        // Parsing the program alone (about 400 KB, one unit per 64 bytes) is past a small budget.
        let tight = ContentDocument::open(file, 2_000).unwrap();
        assert!(matches!(tight.render_upright(0, 300), Err(ContentError::TooComplex)));
    }

    #[test]
    fn a_soft_mask_past_the_budget_is_too_complex() {
        use lopdf::{Object, Stream, dictionary};
        let group = Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form", "Group" => dictionary! { "S" => "Transparency", "CS" => "DeviceGray" },
            "BBox" => vec![Object::Integer(0), Object::Integer(0), Object::Integer(200), Object::Integer(300)] },
            b"1 g 0 0 1 1 re f".to_vec(),
        );
        // Tiny paints: only the mask's canvas costs more than the tight budget below.
        let (bytes, id) = lopdf_page(b"/GS1 gs 0 0 1 rg 0 0 1 1 re f", dictionary! {}, Some(group));
        let (num, generation) = id.unwrap();
        // The page again, now naming the soft mask in its resources.
        let mut doc = lopdf::Document::load_mem(&bytes).unwrap();
        let page = *doc.get_pages().values().next().unwrap();
        let resources = dictionary! { "ExtGState" => dictionary! { "GS1" => dictionary! { "SMask" => dictionary! { "S" => "Luminosity", "G" => Object::Reference((num, generation)) } } } };
        doc.get_object_mut(page).unwrap().as_dict_mut().unwrap().set("Resources", resources);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let file = Arc::new(bytes);
        assert!(ContentDocument::open(file.clone(), 1_000_000).unwrap().render_upright(0, 300).is_ok());
        // The mask alone is a canvas of 200 × 300 pixels: about 230 units.
        assert!(matches!(ContentDocument::open(file, 100).unwrap().render_upright(0, 300), Err(ContentError::TooComplex)));
    }

    /// A 200 × 300 page drawing `content` with a Type 3 font `/T` (glyph "a": a black box over the
    /// advance, 0–700 of its em, `/FontMatrix` as given) and a form `/Fm` drawing `form` under
    /// `/Matrix` `matrix`.
    fn type3_page(content: &str, font_matrix: &str, form: &str, matrix: &str) -> Vec<u8> {
        let font = format!(
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 500 700] /FontMatrix [{font_matrix}] /CharProcs << /a 4 0 R >> \
             /Encoding << /Type /Encoding /Differences [97 /a] >> /FirstChar 97 /LastChar 97 /Widths [500] /Resources << >> >>"
        );
        let glyph = "500 0 0 0 500 700 d1 0 0 500 700 re f";
        let resources = "<< /Font << /T 3 0 R >> /XObject << /Fm 5 0 R >> >>";
        let bodies = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [6 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_string(),
            font,
            format!("<< /Length {} >>\nstream\n{glyph}\nendstream", glyph.len()),
            format!(
                "<< /Type /XObject /Subtype /Form /BBox [0 0 400 400] /Matrix [{matrix}] /Resources {resources} /Length {} >>\nstream\n{form}\nendstream",
                form.len()
            ),
            format!("<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources {resources} >>"),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", bodies.len() + 1).as_bytes());
        out
    }

    /// The glyph boxes the text extraction reports agree with where the same interpreter draws:
    /// every inked pixel of the render lies in a reported box (within a point), and each box's
    /// centre is inked. The boxes come from hayro's own interpretation (`GlyphDevice`), not from
    /// state tracked beside it, so q/Q, cm, text state and form matrices are hayro's.
    #[test]
    fn glyph_boxes_are_where_the_interpreter_draws() {
        let identity = "0.001 0 0 0.001 0 0";
        let cases = [
            ("unbalanced Q", type3_page("Q Q BT /T 20 Tf 30 150 Td (aaa) Tj ET Q", identity, "", "1 0 0 1 0 0")),
            ("q/Q inside BT/ET", type3_page("BT /T 20 Tf 30 150 Td q 2 0 0 2 0 0 cm Q (aa) Tj q (a) Tj Q ET", identity, "", "1 0 0 1 0 0")),
            ("Tz, Ts and TJ", type3_page("BT /T 20 Tf 150 Tz 5 Ts 2 Tc 30 150 Td [(a) -800 (a) 300 (a)] TJ ET", identity, "", "1 0 0 1 0 0")),
            ("nested form with /Matrix", type3_page("q 1 0 0 1 10 20 cm /Fm Do Q", identity, "BT /T 20 Tf 0 0 Td (aa) Tj ET", "0.5 0 0 1.5 40 60")),
            ("Type 3 font matrix", type3_page("BT /T 20 Tf 30 150 Td (aa) Tj ET", "0.002 0 0 0.001 0 0", "", "1 0 0 1 0 0")),
        ];
        for (case, file) in cases {
            let doc = ContentDocument::open(Arc::new(file), 1_000_000).unwrap();
            let glyphs = doc.text(0).unwrap();
            assert!(!glyphs.is_empty(), "{case}");
            let raster = doc.render_upright(0, 600).unwrap();
            let to_user = |px: usize, py: usize| {
                (raster.crop[0] + (px as f32 + 0.5) / raster.scale, raster.crop[1] + raster.height_pt - (py as f32 + 0.5) / raster.scale)
            };
            let boxes: Vec<[f32; 4]> = (0..glyphs.len()).map(|i| glyphs.glyph(i).unwrap().1).collect();
            let inked = |px: usize, py: usize| raster.rgba.get((py * raster.width as usize + px) * 4).is_some_and(|r| *r < 128);
            for py in 0..raster.height as usize {
                for px in 0..raster.width as usize {
                    if inked(px, py) {
                        let (x, y) = to_user(px, py);
                        assert!(
                            boxes.iter().any(|b| x >= b[0] - 1.0 && x <= b[2] + 1.0 && y >= b[1] - 1.0 && y <= b[3] + 1.0),
                            "{case}: ink at {x},{y} outside {boxes:?}"
                        );
                    }
                }
            }
            for b in &boxes {
                // The glyph's ink is its advance by 0–700 of the em; its box spans −200–800.
                let (x, y) = ((b[0] + b[2]) / 2.0, b[1] + (b[3] - b[1]) * 0.45);
                let px = ((x - raster.crop[0]) * raster.scale) as usize;
                let py = ((raster.height_pt - (y - raster.crop[1])) * raster.scale) as usize;
                assert!(inked(px, py), "{case}: no ink at the box {b:?}");
            }
        }
    }
}
