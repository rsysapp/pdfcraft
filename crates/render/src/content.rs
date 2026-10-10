//! Embed build: what a save inspector reads through hayro: the page count, each page's annotation
//! count, its text (page content only) and an upright render of its content (no annotations,
//! `/Rotate` ignored). Every answer is certain or an error: a glyph that gives no text, an
//! interpreter warning (an unsupported font, an image that does not decode) or a renderer panic
//! fails the call instead of being skipped, and [`ContentDocument::repaired`] says whether the
//! parser had to rebuild the cross-reference data.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::object::{Array, Dict};
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, render_upright};

use crate::raster::{RenderConfig, panic_message};

/// Why a file did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentOpenError {
    /// The file is encrypted (a password is needed, or its security is not supported).
    Encrypted,
    /// The file is not a PDF hayro can read.
    Invalid,
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
}

impl ContentDocument {
    /// Open `bytes` with the empty password. An encrypted file is [`ContentOpenError::Encrypted`]
    /// whether or not the empty password opens it: hayro's typed decryption errors say which.
    pub fn open(bytes: Arc<Vec<u8>>) -> Result<Self, ContentOpenError> {
        let opened = catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(bytes.clone(), "")));
        let pdf = match opened {
            Ok(Ok(pdf)) => pdf,
            Ok(Err(LoadPdfError::Decryption(_))) => return Err(ContentOpenError::Encrypted),
            Ok(Err(LoadPdfError::Invalid)) | Err(_) => return Err(ContentOpenError::Invalid),
        };
        let warnings = Arc::new(AtomicUsize::new(0));
        let counter = warnings.clone();
        let settings = InterpreterSettings {
            // Page content only: annotations are compared object by object, not drawn.
            render_annotations: false,
            warning_sink: Arc::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
            ..RenderConfig::default().settings()
        };
        Ok(Self { bytes, pdf, settings, warnings })
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

    /// The page's text in reading order, without whitespace. Fails when a drawn glyph gives no
    /// text (no Unicode mapping, or no usable box), on an interpreter warning, and on a panic.
    pub fn text(&self, page: usize) -> Result<Vec<char>, String> {
        let before = self.warnings();
        let extracted = catch_unwind(AssertUnwindSafe(|| crate::text::extract_page_counted(&self.pdf, page, &self.settings)));
        let (text, unread) = match extracted {
            Ok(Some(found)) => found,
            Ok(None) => return Err(format!("page {} does not exist", page.saturating_add(1))),
            Err(panic) => return Err(format!("text extraction crashed on page {}: {}", page.saturating_add(1), panic_message(&panic))),
        };
        if unread > 0 {
            return Err(format!("page {}: {unread} glyph(s) have no text", page.saturating_add(1)));
        }
        if self.warnings() != before {
            return Err(format!("page {}: the interpreter reported a problem", page.saturating_add(1)));
        }
        Ok(text.glyphs.iter().flat_map(|glyph| glyph.text.chars()).filter(|c| !c.is_whitespace()).collect())
    }

    /// The page box hayro renders upright (intersected crop box), in points: width, height.
    pub fn upright_size(&self, page: usize) -> Option<(f32, f32)> {
        self.pdf.pages().get(page).map(|page| page.base_dimensions())
    }

    /// Render the page's content upright, at the scale that makes its longer side `long_side`
    /// pixels. Fails on an interpreter warning, on a panic, and when the parser has to rebuild
    /// the cross-reference data meanwhile.
    ///
    /// Each render reads the file afresh: hayro keeps what it decoded for a page as long as its
    /// document lives (tens of megabytes for a page of heavy content), so rendering every page of
    /// one document would hold all of it at once.
    pub fn render_upright(&self, page: usize, long_side: u32) -> Result<UprightRaster, String> {
        let before = self.warnings();
        let number = page.saturating_add(1);
        let pdf = match catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(self.bytes.clone(), ""))) {
            Ok(Ok(pdf)) => pdf,
            _ => return Err("the file could not be parsed again for rendering".into()),
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
            Err(panic) => return Err(format!("renderer crashed on page {number}: {}", panic_message(&panic))),
        };
        if self.warnings() != before {
            return Err(format!("page {number}: the interpreter reported a problem"));
        }
        if pdf.xref().is_repaired() {
            return Err("the file's cross-reference data is damaged".into());
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
        let sound = ContentDocument::open(Arc::new(file(PAGE, 0))).unwrap();
        assert!(!sound.repaired());
        assert_eq!(sound.page_count(), 1);
        // `startxref` points into nothing: hayro rebuilds the table by scanning.
        let damaged = ContentDocument::open(Arc::new(file(PAGE, 3))).unwrap();
        assert!(damaged.repaired());
    }

    #[test]
    fn renders_a_rotated_page_upright() {
        let doc = ContentDocument::open(Arc::new(file("<< /Type /Page /Parent 2 0 R /Rotate 90 >>", 0))).unwrap();
        assert!(!doc.repaired());
        let raster = doc.render_upright(0, 300).unwrap();
        // Upright: 200 × 300 points, longer side 300 pixels, whatever `/Rotate` says.
        assert_eq!((raster.width, raster.height), (200, 300));
    }
}
