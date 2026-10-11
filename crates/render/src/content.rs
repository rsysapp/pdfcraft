//! Embed build: what a save inspector reads through hayro: the page count, each page's annotation
//! count, its text (page content only) and an upright render of its content (no annotations,
//! `/Rotate` ignored). Every answer is certain or an error: a glyph that gives no text, an
//! interpreter warning (an unsupported font, an image that does not decode) or a renderer panic
//! fails the call instead of being skipped, and [`ContentDocument::repaired`] says whether the
//! parser had to rebuild the cross-reference data.
//!
//! Work is bounded without a clock: each interpretation (a page's text, a page's render) may do at
//! most the work budget given to [`ContentDocument::open`] (hayro's `work_budget`: operators,
//! glyphs, and paints weighed by the device area they cover); past it the call is
//! [`ContentError::TooComplex`]. Stream decoding (font programs, images, every stream the page
//! needs) is bounded per interpretation too (`hayro_syntax::budget`, set by the interpreter), and
//! running out of it is the same. [`ContentDocument::work_spent`] totals the work done, decoding
//! included, so a caller can bound a whole document too.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, InterpreterWarning, Paint, PathDrawMode,
    SoftMask, TransformExt, interpret_page, work_spent,
};
use hayro::hayro_syntax::budget::{decode_budget_exceeded, decode_spent, set_decode_budget};
use hayro::hayro_syntax::object::{Array, Dict};
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, render_upright};
use kurbo::{BezPath, Shape};

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
/// advance by the em square (descender −200 to ascender 800 of 1,000), as the font places it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageGlyphs {
    /// Every glyph's characters, one after the other.
    pub chars: Vec<char>,
    /// Per glyph: where its characters end in `chars`, and its box `[x0, y0, x1, y1]`.
    pub glyphs: Vec<(u32, [f32; 4])>,
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

/// Records every glyph drawn with its text and its em box in user space.
struct GlyphDevice {
    /// View space (where the interpreter draws) to user space.
    to_user: kurbo::Affine,
    found: PageGlyphs,
    /// Glyphs drawn that gave no text (no Unicode value) or no usable box.
    unread: usize,
}

impl<'a> Device<'a> for GlyphDevice {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: kurbo::Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph(&mut self, glyph: &Glyph<'a>, transform: kurbo::Affine, glyph_transform: kurbo::Affine, _: &Paint<'a>, _: &GlyphDrawMode) {
        let Some(unicode) = glyph.as_unicode() else {
            self.unread = self.unread.saturating_add(1);
            return;
        };
        let text = match unicode {
            hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
            hayro::hayro_interpret::hayro_cmap::BfString::String(s) => s,
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
        self.found.chars.extend(text.chars());
        let end = u32::try_from(self.found.chars.len()).unwrap_or(u32::MAX);
        self.found.glyphs.push((end, [b.x0 as f32, b.y0 as f32, b.x1 as f32, b.y1 as f32]));
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: kurbo::Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
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
}

impl ContentDocument {
    /// Open `bytes` with the empty password. An encrypted file is [`ContentOpenError::Encrypted`]
    /// whether or not the empty password opens it: hayro's typed decryption errors say which.
    /// `work_budget` bounds each interpretation (see the module documentation).
    pub fn open(bytes: Arc<Vec<u8>>, work_budget: u64) -> Result<Self, ContentOpenError> {
        let opened = catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(bytes.clone(), "")));
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
            work_budget,
            warning_sink: Arc::new(move |warning| {
                let counter = if matches!(warning, InterpreterWarning::WorkBudgetExceeded) { &budget_counter } else { &counter };
                counter.fetch_add(1, Ordering::Relaxed);
            }),
            ..RenderConfig::default().settings()
        };
        Ok(Self { bytes, pdf, settings, warnings, exhausted, spent: AtomicU64::new(0) })
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

    /// The glyphs the page draws (see [`PageGlyphs`]). Fails when a drawn glyph gives no text (no
    /// Unicode mapping) or no usable box, on an interpreter warning, on a panic, when the parser
    /// had to rebuild the cross-reference data, and past the work budget.
    pub fn text(&self, page: usize) -> Result<PageGlyphs, ContentError> {
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
            let mut device = GlyphDevice { to_user: initial.inverse(), found: PageGlyphs::default(), unread: 0 };
            interpret_page(p, &mut ctx, &mut device);
            Some(Ok((device.found, device.unread)))
        }));
        let (found, unread) = match extracted {
            Ok(Some(Ok(found))) => found,
            Ok(Some(Err(()))) => return Err(format!("page {number} has no usable page box").into()),
            Ok(None) => return Err(format!("page {number} does not exist").into()),
            Err(panic) => return Err(format!("text extraction crashed on page {number}: {}", panic_message(&panic)).into()),
        };
        self.finish(number, before)?;
        if unread > 0 {
            return Err(format!("page {number}: {unread} glyph(s) have no text").into());
        }
        if self.repaired() {
            return Err(ContentError::Unreadable("the file's cross-reference data is damaged".into()));
        }
        Ok(found)
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
            let settings = RenderSettings {
                x_scale: scale,
                y_scale: scale,
                width: u16::try_from(width).ok(),
                height: u16::try_from(height).ok(),
                bg_color: WHITE,
                ..Default::default()
            };
            let cache = RenderCache::new();
            let pixmap = render_upright(page, &cache, &self.settings, &settings);
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
}
