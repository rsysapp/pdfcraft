//! Embed build: compare a saved PDF with the file it was edited from. A host checks every save from
//! the embed build with this. Facts only; what is acceptable is the host's rule.
//!
//! **Every fact is certain, or the call fails.** A host refuses an edited file on any error, so
//! failing closed is always safe; reporting a fact that could not be established (an unreadable
//! page as "no text", an image that could not be hashed as "no image") would let a forged save
//! through. So:
//! - structure (page tree, page dictionaries, annotations, form fields, the cross-reference chain
//!   and the trailer) comes from one parser, `pdfcraft_cos`, the one the writer uses; every object
//!   read must sit where the cross-reference data says, and any repair is an error;
//! - hayro (through `pdfcraft_render::content`) only extracts text and renders, and must agree with
//!   `pdfcraft_cos` on the page count and each page's annotation count; a glyph without text, an
//!   interpreter warning or a rebuilt cross-reference table is an error;
//! - an appended update must leave every base page's content and resources as they were (edit
//!   mode never writes page content there); a rewrite is accepted only as an applied redaction,
//!   checked visually (every changed pixel lies inside a uniformly filled black rectangle), glyph
//!   by glyph (a glyph is its text and its place; every removed glyph lies under a box, and none
//!   is added) and image by image at each image's own resolution (pixels outside the boxes are
//!   the base's, or within requantisation of them for a JPEG);
//! - work is bounded (pages, decoded bytes, hashed bytes, rendered pixels, nesting, a deadline),
//!   and exceeding a bound is [`InspectError::TooComplex`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use pdfcraft_cos::{CosError, Document, ObjRef};
use pdfcraft_render::content::{ContentDocument, ContentError, ContentOpenError, UprightRaster};

mod glyphs;
mod images;
mod jpeg;
mod structure;
mod visual;

use images::{Areas, Images};
use structure::{Annot, Digest, PageFacts, Structure};

/// A longer document is refused rather than compared slowly.
pub const MAX_PAGES: usize = 2_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inspection {
    /// The edited file starts with the base's bytes: the edit is an appended update.
    pub base_kept: bool,
    pub pages: PageChanges,
    /// Form fields (by full name) whose value, field dictionary or widgets changed, appeared or
    /// went, plus one when the form's own dictionary (`/AcroForm` without `/Fields`) changed.
    /// A widget's appearance changing with its value unchanged counts here (it can show other
    /// text than the value), as does `/NeedAppearances` (it lets viewers redraw every field):
    /// both are form changes, not annotations.
    pub form_fields_changed: u32,
    pub annotations: AnnotationChanges,
    pub text: TextChanges,
    pub content: ContentChanges,
    /// Rewrites only (empty for appended updates): the applied redaction boxes that explain the
    /// changes, as (page number in the edited file, from 1; rectangle `[x0, y0, x1, y1]` in that
    /// page's user space, PDF points). Boxes that overlap (the line boxes of one mark) are one
    /// area, given as the fewest row bands; each is accurate to a pixel of the comparison render
    /// (at most about half a point on a letter or A4 page). Pages counted in
    /// `content.pages_changed_outside_redactions` contribute none. Ordered by page, then top to
    /// bottom. More than [`MAX_REDACTION_AREAS`] is [`InspectError::TooComplex`].
    pub redaction_areas: Vec<(u32, [f32; 4])>,
}

/// The most redaction areas an inspection reports.
pub const MAX_REDACTION_AREAS: usize = 10_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageChanges {
    pub before: u32,
    pub after: u32,
    /// Pages present in both files whose `/Rotate` differs.
    pub rotated: u32,
    pub inserted: u32,
    pub deleted: u32,
    /// Pages found, unchanged, at another position.
    pub moved: u32,
}

/// Annotations (form-field widgets are form fields; pop-ups without an appearance belong to their
/// markup and are not counted). Added ones are counted by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnnotationChanges {
    /// Added `Stamp` annotations.
    pub stamps: u32,
    /// Added ink, line, square, circle, polygon and polyline annotations.
    pub drawings: u32,
    /// Added notes, text boxes, text markup, carets, file attachments and sounds.
    pub comments: u32,
    /// Added `Redact` annotations (marks not applied). Marks kept from the base are not counted.
    pub redaction_marks: u32,
    /// Added annotations of any other kind (links, widgets outside the form, pop-ups with an
    /// appearance, unknown subtypes).
    pub other: u32,
    /// Base annotations gone. In an appended update, by object identity; in a rewrite (which
    /// renumbers objects), a base annotation whose dictionary is no longer on its page.
    pub removed: u32,
    /// Appended updates: base annotations (by object identity) whose dictionary or appearance
    /// changed, or that moved to another page. (A rewrite reports these as removed and added.)
    pub changed: u32,
    /// Rewrites: base `Redact` marks gone from a page where their area is now uniformly filled.
    pub redactions_applied: u32,
}

/// Among pages present in both files, glyph by glyph: a glyph is its text and its place (its em
/// box, within half a point on every side), and glyphs drawn as spaces count like any other.
/// Pages where a base glyph has no edited glyph with the same text at the same place (text
/// removed); pages where an edited glyph has no base glyph with the same text at the same place
/// (text added: moved or new text). A redaction removes text and adds none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextChanges {
    pub pages_with_text_removed: u32,
    pub pages_with_text_added: u32,
}

/// Among pages present in both files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentChanges {
    /// Pages that lost an image (drawn directly, inside a form XObject, or inline). Appended
    /// updates compare images by a hash of each one's encoded stream (a cleared image is a new
    /// one). Rewrites compare each image placement: a base image gone, or replaced at its place by
    /// one whose pixels differ under a redaction area (an image redacted in place).
    pub pages_with_images_removed: u32,
    /// Pages that gained an image. In rewrites, an image redacted in place (the same size and
    /// meaning at the same place, pixels changed only under the redaction areas) is not one.
    pub pages_with_images_added: u32,
    /// Pages that lost text or an image, each page once.
    pub pages_with_content_removed: u32,
    /// Appended updates only (0 for rewrites): pages whose content streams, resources or other
    /// page-dictionary entries (all but `/Rotate` and `/Annots`) differ from the base page's, or
    /// every page present in both when the document's layer configuration changed.
    pub pages_with_content_changed: u32,
    /// Rewrites only (0 for appended updates): pages whose content changed anywhere but inside
    /// the editor's redaction boxes: opaque, uniformly filled rectangles in the redaction fill
    /// colour (black), found in the page rendered upright without annotations. A deletion under a
    /// box of any other colour, or a translucent one, counts here, and so does, visible or not:
    /// - a removed glyph (see [`TextChanges`]) whose em box's centre lies outside every box;
    /// - an image changed at its own resolution outside the boxes: a pixel whose centre lies
    ///   outside every box that differs from the base (losslessly coded images), or differs by
    ///   more than requantising the base with the edited image's quantisation tables explains
    ///   (baseline JPEGs); an image replaced by one of another size, meaning, coding or placement,
    ///   or removed, unless it lay wholly under the boxes.
    pub pages_changed_outside_redactions: u32,
}

/// Why a comparison has no answer. The host refuses the edited file on every one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectError {
    /// The base could not be read (the host's own stored version: an operator problem).
    Base(String),
    /// The edited file could not be read, or a fact about it could not be established.
    Edited(String),
    /// The edited file is encrypted (output encryption is refused by the host).
    Encrypted,
    /// A work bound or the deadline was exceeded.
    TooComplex,
}

/// How long an inspection may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectLimits {
    /// Wall-clock time for the whole comparison, checked between units of work (a page's text,
    /// a page render, a batch of objects). Not measured on wasm32, which has no clock here.
    /// The work bounds hold on every target and do not depend on a clock. They are what bounds
    /// the time on wasm32: each page's text and render, and the comparison as a whole, may do
    /// only a fixed amount of interpreter work, at most about half a second a page and half a
    /// minute in all, as measured natively.
    pub deadline: Duration,
}

impl Default for InspectLimits {
    fn default() -> Self {
        Self { deadline: Duration::from_secs(60) }
    }
}

/// Compare `edited` with the `base` it was edited from.
pub fn inspect(base: &[u8], edited: &[u8], limits: InspectLimits) -> Result<Inspection, InspectError> {
    inspect_with(base, edited, Limits { deadline: limits.deadline, ..Limits::default() })
}

/// Every bound (the public [`InspectLimits`] sets only the deadline).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    // Embed build: wasm32 has no clock here, so the deadline goes unread there (the wasm32 clippy gate).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub deadline: Duration,
    /// Content-stream bytes decoded (page and form contents, object streams), in both files, plus
    /// the content bytes each page makes the interpreter read (per text extraction and render).
    pub content_bytes: usize,
    /// Stream bytes hashed (images and every other stream reached from a page), in both files.
    pub hashed_bytes: usize,
    /// Pixels rendered for the visual comparison.
    pub rendered_pixels: u64,
    /// Form XObjects inside form XObjects.
    pub form_depth: usize,
    /// Objects read (a cross-reference entry checked and loaded), in both files.
    pub objects: usize,
    /// XObject and inline-image uses walked, in both files.
    pub uses: usize,
    /// Image pixels a page may draw (sum over its uses) for it to be rendered.
    pub page_image_pixels: u64,
    /// Content bytes a page may make the interpreter read (forms counted per use) for it to be
    /// rendered: rendering heavy vector content takes tens of megabytes a page.
    pub page_render_content: usize,
    /// The longer side of a page render, in pixels.
    pub render_side: u32,
    /// The work one interpretation (a page's text, a page's render) may do, in hayro's work units:
    /// one per operator and glyph, and each fill, stroke, image or shading by its device area
    /// (one unit per 256 pixels). Measured: about 0.7 µs a unit at worst (tiny fills), so a page
    /// is bounded to well under a second whatever it draws, with or without a clock.
    pub page_work: u64,
    /// The work all interpretations of both files may do together.
    pub work: u64,
    /// Glyphs kept (text and place, for the glyph pairing), in both files.
    pub glyphs: usize,
    /// Bytes of image samples (or JPEG coefficients) decoded for a rewrite's image comparison,
    /// in all, and for one image.
    pub image_bytes: usize,
    pub image_bytes_each: usize,
    /// Image samples compared one by one.
    pub compared: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            deadline: InspectLimits::default().deadline,
            content_bytes: 256 << 20,
            hashed_bytes: 512 << 20,
            rendered_pixels: MAX_PAGES as u64 * 1_600 * 1_600,
            form_depth: 16,
            objects: 4_000_000,
            uses: 1_000_000,
            page_image_pixels: 1 << 26,
            page_render_content: 4 << 20,
            render_side: 1_600,
            page_work: 1_000_000,
            work: 64_000_000,
            glyphs: 8_000_000,
            image_bytes: 1 << 30,
            image_bytes_each: 64 << 20,
            compared: 1 << 30,
        }
    }
}

/// Internal failure, before it is attributed to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fail {
    Read(String),
    Encrypted,
    TooComplex,
}

/// Which file a failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Base,
    Edited,
}

impl Fail {
    fn of(self, role: Role) -> InspectError {
        match (self, role) {
            (Fail::TooComplex, _) => InspectError::TooComplex,
            (Fail::Encrypted, Role::Edited) => InspectError::Encrypted,
            (Fail::Encrypted, Role::Base) => InspectError::Base("the file is encrypted".into()),
            (Fail::Read(message), Role::Base) => InspectError::Base(message),
            (Fail::Read(message), Role::Edited) => InspectError::Edited(message),
        }
    }
}

/// The work left, shared by both files.
pub(crate) struct Budget {
    #[cfg(not(target_arch = "wasm32"))]
    deadline: Option<std::time::Instant>,
    content_left: usize,
    hashed_left: usize,
    pixels_left: u64,
    objects_left: usize,
    uses_left: usize,
    work_left: u64,
    glyphs_left: usize,
    image_bytes_left: usize,
    compared_left: u64,
    ticks: u32,
    pub limits: Limits,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            deadline: std::time::Instant::now().checked_add(limits.deadline),
            content_left: limits.content_bytes,
            hashed_left: limits.hashed_bytes,
            pixels_left: limits.rendered_pixels,
            objects_left: limits.objects,
            uses_left: limits.uses,
            work_left: limits.work,
            glyphs_left: limits.glyphs,
            image_bytes_left: limits.image_bytes,
            compared_left: limits.compared,
            ticks: 0,
            limits,
        }
    }

    /// Fail when the deadline has passed.
    pub fn time(&mut self) -> Result<(), Fail> {
        #[cfg(not(target_arch = "wasm32"))]
        if self.deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(Fail::TooComplex);
        }
        Ok(())
    }

    /// A small unit of work: the deadline is checked every 1,024 of them.
    pub fn tick(&mut self) -> Result<(), Fail> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(1_024) { self.time() } else { Ok(()) }
    }

    fn take(left: &mut usize, amount: usize) -> Result<(), Fail> {
        *left = left.checked_sub(amount).ok_or(Fail::TooComplex)?;
        Ok(())
    }

    /// Bytes a decoder may still produce.
    pub fn content_room(&self) -> usize {
        self.content_left
    }

    pub fn content(&mut self, bytes: usize) -> Result<(), Fail> {
        Self::take(&mut self.content_left, bytes)
    }

    pub fn hashed(&mut self, bytes: usize) -> Result<(), Fail> {
        Self::take(&mut self.hashed_left, bytes)?;
        self.tick()
    }

    pub fn object(&mut self) -> Result<(), Fail> {
        Self::take(&mut self.objects_left, 1)?;
        self.tick()
    }

    pub fn uses(&mut self) -> Result<(), Fail> {
        Self::take(&mut self.uses_left, 1)?;
        self.tick()
    }

    /// Charge the work `hayro` did since it had done `before`.
    fn work(&mut self, hayro: &ContentDocument, before: u64) -> Result<(), Fail> {
        let done = hayro.work_spent().saturating_sub(before);
        self.work_left = self.work_left.checked_sub(done).ok_or(Fail::TooComplex)?;
        Ok(())
    }

    fn pixels(&mut self, pixels: u64) -> Result<(), Fail> {
        self.pixels_left = self.pixels_left.checked_sub(pixels).ok_or(Fail::TooComplex)?;
        Ok(())
    }

    fn glyphs(&mut self, count: usize) -> Result<(), Fail> {
        Self::take(&mut self.glyphs_left, count)
    }

    /// Bytes one image's decoded samples may still take.
    pub fn image_room(&self) -> usize {
        self.image_bytes_left.min(self.limits.image_bytes_each)
    }

    pub fn image_bytes(&mut self, bytes: usize) -> Result<(), Fail> {
        Self::take(&mut self.image_bytes_left, bytes)?;
        self.time()
    }

    pub fn compared(&mut self, samples: usize) -> Result<(), Fail> {
        self.compared_left = self.compared_left.checked_sub(samples as u64).ok_or(Fail::TooComplex)?;
        self.tick()
    }
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// What one file says: its structure (from `pdfcraft_cos`) and its pages' text (from hayro).
struct Side {
    structure: Structure,
    hayro: ContentDocument,
    /// The file again, for decoding images a rewrite replaced.
    images: Images,
}

/// The whole comparison, with every bound.
pub(crate) fn inspect_with(base: &[u8], edited: &[u8], limits: Limits) -> Result<Inspection, InspectError> {
    // A last resort: nothing below may panic, but a panic must not pass for a result either.
    match catch_unwind(AssertUnwindSafe(|| compare(base, edited, limits))) {
        Ok(result) => result,
        Err(_) => Err(InspectError::Edited("internal error while inspecting the file".into())),
    }
}

fn compare(base: &[u8], edited: &[u8], limits: Limits) -> Result<Inspection, InspectError> {
    let mut budget = Budget::new(limits);
    budget.time().map_err(|fail| fail.of(Role::Base))?;
    let base_kept = edited.starts_with(base);
    // A rewrite's image placements are compared against its redaction boxes.
    let before = read_side(base, None, !base_kept, &mut budget).map_err(|fail| fail.of(Role::Base))?;
    let after = read_side(edited, Some((base, &before.structure, base_kept)), !base_kept, &mut budget).map_err(|fail| fail.of(Role::Edited))?;
    if let Some(key) = structure::catalog_change(&before.structure, &after.structure) {
        return Err(InspectError::Edited(format!("the document's /{key} changed, which this comparison does not report")));
    }
    let mut inspection = Inspection { base_kept, ..Inspection::default() };
    let pairs =
        match_pages(&before.structure.pages, &after.structure.pages, &mut inspection.pages, &mut budget).map_err(|fail| fail.of(Role::Edited))?;
    // The document's layer configuration decides what every page shows.
    let layers_changed = before.structure.layers != after.structure.layers;
    inspection.form_fields_changed = fields_changed(&before.structure, &after.structure);
    let mut rasters = Rasters::default();
    compare_pages(&before, &after, &pairs, base_kept, layers_changed, &mut budget, &mut rasters, &mut inspection)?;
    inspection.redaction_areas.sort_by(|(page_a, a), (page_b, b)| page_a.cmp(page_b).then(b[3].total_cmp(&a[3])).then(a[0].total_cmp(&b[0])));
    inspection.annotations = compare_annotations(&before, &after, &pairs, base_kept, &mut budget, &mut rasters)?;
    Ok(inspection)
}

/// Read one file. `against` is the base (bytes, structure, whether `bytes` starts with it) when
/// reading the edited file: its cross-reference chain is checked against the base's. `placements`
/// keeps where each image was drawn.
fn read_side(bytes: &[u8], against: Option<(&[u8], &Structure, bool)>, placements: bool, budget: &mut Budget) -> Result<Side, Fail> {
    let shared = Arc::new(bytes.to_vec());
    let doc = match catch_unwind(AssertUnwindSafe(|| Document::open(shared.clone()))) {
        Ok(Ok(doc)) => doc,
        Ok(Err(CosError::NeedsPassword | CosError::WrongPassword | CosError::Security(_))) => return Err(Fail::Encrypted),
        Ok(Err(error)) => return Err(Fail::Read(error.to_string())),
        Err(_) => return Err(Fail::Read("the file could not be parsed".into())),
    };
    if doc.security().is_some() || doc.trailer().contains(b"Encrypt") {
        return Err(Fail::Encrypted);
    }
    if let Some(repair) = doc.repair_log().first() {
        return Err(Fail::Read(format!("the file is damaged: {repair}")));
    }
    let structure = structure::read(&doc, budget)?;
    if let Some((base_bytes, base, base_kept)) = against {
        structure::check_chain(&structure.sections, base_bytes.len(), &base.sections, base_kept)?;
    }
    drop(doc);
    let hayro = match ContentDocument::open(shared.clone(), budget.limits.page_work) {
        Ok(hayro) => hayro,
        Err(ContentOpenError::Encrypted) => return Err(Fail::Encrypted),
        Err(ContentOpenError::Invalid) => return Err(Fail::Read("the file could not be parsed for its content".into())),
    };
    if hayro.page_count() != structure.pages.len() {
        return Err(Fail::Read(format!("readers disagree on the page count ({} or {})", structure.pages.len(), hayro.page_count())));
    }
    let mut side = Side { structure, hayro, images: Images::new(shared) };
    for (index, page) in side.structure.pages.iter_mut().enumerate() {
        budget.time()?;
        let number = index.saturating_add(1);
        if side.hayro.annotation_count(index) != Some(page.annots_in_array) {
            return Err(Fail::Read(format!("readers disagree on the annotations of page {number}")));
        }
        page.size = side.hayro.upright_size(index).ok_or_else(|| Fail::Read(format!("page {number} could not be read")))?;
        budget.content(page.interpreted)?;
        let before = side.hayro.work_spent();
        let text = side.hayro.text(index);
        budget.work(&side.hayro, before)?;
        let glyphs = text.map_err(content_failure)?;
        budget.glyphs(glyphs.len().saturating_add(glyphs.chars.len()))?;
        page.glyphs = glyphs;
        if !placements {
            page.image_uses = Vec::new();
        }
        page.fingerprint = structure::fingerprint(page);
    }
    if side.hayro.repaired() {
        return Err(Fail::Read("the file's cross-reference data is damaged".into()));
    }
    Ok(side)
}

fn content_failure(error: ContentError) -> Fail {
    match error {
        ContentError::Unreadable(message) => Fail::Read(message),
        ContentError::TooComplex => Fail::TooComplex,
    }
}

/// Pages present in both files: (base page, edited page).
type Pairs = Vec<(usize, usize)>;

/// Longest common subsequence of page fingerprints, as (base page, edited page) pairs.
fn in_order(a: &[Digest], b: &[Digest]) -> Pairs {
    let width = b.len().saturating_add(1);
    let mut table = vec![0u16; a.len().saturating_add(1).saturating_mul(width)];
    let cell = |table: &[u16], i: usize, j: usize| table.get(i.saturating_mul(width).saturating_add(j)).copied().unwrap_or(0);
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            let value = if a.get(i) == b.get(j) {
                cell(&table, i.saturating_add(1), j.saturating_add(1)).saturating_add(1)
            } else {
                cell(&table, i.saturating_add(1), j).max(cell(&table, i, j.saturating_add(1)))
            };
            if let Some(slot) = table.get_mut(i.saturating_mul(width).saturating_add(j)) {
                *slot = value;
            }
        }
    }
    let (mut i, mut j, mut pairs) = (0usize, 0usize, Vec::new());
    while i < a.len() && j < b.len() {
        if a.get(i) == b.get(j) {
            pairs.push((i, j));
            i = i.saturating_add(1);
            j = j.saturating_add(1);
        } else if cell(&table, i.saturating_add(1), j) >= cell(&table, i, j.saturating_add(1)) {
            i = i.saturating_add(1);
        } else {
            j = j.saturating_add(1);
        }
    }
    pairs
}

/// An edited page could be the base page after a redaction: same size, and every glyph it draws
/// is a base glyph (same text, same place).
fn could_be_redacted(base: &PageFacts, edited: &PageFacts, budget: &mut Budget) -> Result<bool, Fail> {
    let same = |x: f32, y: f32| (x - y).abs() < 0.01;
    if !(same(base.size.0, edited.size.0) && same(base.size.1, edited.size.1)) || edited.glyphs.len() > base.glyphs.len() {
        return Ok(false);
    }
    Ok(glyphs::pair(&base.glyphs, &edited.glyphs, budget)?.added.is_empty())
}

/// Pair the pages: identical pages in order, then identical pages elsewhere (moved), then the
/// rest — pages that could be a redaction of the base page first, then by position. What is
/// left was inserted or deleted.
fn match_pages(before: &[PageFacts], after: &[PageFacts], changes: &mut PageChanges, budget: &mut Budget) -> Result<Pairs, Fail> {
    let base_prints: Vec<Digest> = before.iter().map(|page| page.fingerprint).collect();
    let edited_prints: Vec<Digest> = after.iter().map(|page| page.fingerprint).collect();
    let mut pairs = in_order(&base_prints, &edited_prints);
    let used_base: BTreeSet<usize> = pairs.iter().map(|pair| pair.0).collect();
    let used_edited: BTreeSet<usize> = pairs.iter().map(|pair| pair.1).collect();
    let mut base_free: Vec<usize> = (0..before.len()).filter(|page| !used_base.contains(page)).collect();
    let mut edited_free: Vec<usize> = (0..after.len()).filter(|page| !used_edited.contains(page)).collect();
    let mut moved = 0usize;
    base_free.retain(|&page| {
        let Some(position) = edited_free.iter().position(|&other| base_prints.get(page) == edited_prints.get(other)) else { return true };
        pairs.push((page, edited_free.remove(position)));
        moved = moved.saturating_add(1);
        false
    });
    // Changed pages, in order: a page that could be a redaction of the base page first.
    let mut last = None::<usize>;
    let mut unpaired = Vec::new();
    for page in base_free {
        let mut found = None;
        for (position, &other) in edited_free.iter().enumerate() {
            if last.is_some_and(|last| other <= last) {
                continue;
            }
            let (Some(b), Some(e)) = (before.get(page), after.get(other)) else { continue };
            if could_be_redacted(b, e, budget)? {
                found = Some(position);
                break;
            }
        }
        match found {
            Some(position) => {
                let other = edited_free.remove(position);
                last = Some(other);
                pairs.push((page, other));
            }
            None => unpaired.push(page),
        }
    }
    let base_free = unpaired;
    let positional: Vec<(usize, usize)> = base_free.iter().copied().zip(edited_free.iter().copied()).collect();
    pairs.extend(positional.iter().copied());
    *changes = PageChanges {
        before: count(before.len()),
        after: count(after.len()),
        rotated: count(pairs.iter().filter(|(old, new)| before.get(*old).map(|p| p.rotation) != after.get(*new).map(|p| p.rotation)).count()),
        inserted: count(edited_free.len().saturating_sub(positional.len())),
        deleted: count(base_free.len().saturating_sub(positional.len())),
        moved: count(moved),
    };
    pairs.sort_unstable();
    Ok(pairs)
}

/// Whether an image of the multiset `a` is missing from the multiset `b`.
fn images_missing(a: &[Digest], b: &[Digest]) -> bool {
    let mut counts: BTreeMap<&Digest, i64> = BTreeMap::new();
    for image in a {
        *counts.entry(image).or_default() += 1;
    }
    for image in b {
        *counts.entry(image).or_default() -= 1;
    }
    counts.values().any(|value| *value > 0)
}

/// The renders of pages compared visually.
type RasterPair = (Arc<UprightRaster>, Arc<UprightRaster>);

/// Renders kept for the redaction-mark check, only of base pages that have a `Redact` mark (a
/// page's renders take megabytes; a document's would take gigabytes).
#[derive(Default)]
struct Rasters {
    /// (base page, edited page) → (base render, edited render).
    pairs: HashMap<(usize, usize), RasterPair>,
}

impl Rasters {
    fn get(&mut self, before: &Side, after: &Side, pair: (usize, usize), budget: &mut Budget) -> Result<RasterPair, InspectError> {
        if let Some(found) = self.pairs.get(&pair) {
            return Ok(found.clone());
        }
        let base = Arc::new(render(before, pair.0, budget).map_err(|fail| fail.of(Role::Base))?);
        let edited = Arc::new(render(after, pair.1, budget).map_err(|fail| fail.of(Role::Edited))?);
        let marked = before.structure.pages.get(pair.0).is_some_and(|page| page.annots.iter().any(|annot| annot.subtype == b"Redact"));
        if marked {
            self.pairs.insert(pair, (base.clone(), edited.clone()));
        }
        Ok((base, edited))
    }
}

fn render(side: &Side, index: usize, budget: &mut Budget) -> Result<UprightRaster, Fail> {
    budget.time()?;
    let page = side.structure.pages.get(index).ok_or_else(|| Fail::Read("a page went missing".into()))?;
    if page.image_pixels > budget.limits.page_image_pixels || page.interpreted > budget.limits.page_render_content {
        return Err(Fail::TooComplex);
    }
    let side_px = u64::from(budget.limits.render_side);
    budget.pixels(side_px.saturating_mul(side_px))?;
    budget.content(page.interpreted)?;
    let before = side.hayro.work_spent();
    let raster = side.hayro.render_upright(index, budget.limits.render_side);
    budget.work(&side.hayro, before)?;
    raster.map_err(content_failure)
}

/// Per-page counts, in [`ContentChanges`] and [`TextChanges`] order.
#[derive(Default)]
struct PageCounts {
    text_removed: usize,
    text_added: usize,
    images_removed: usize,
    images_added: usize,
    content_removed: usize,
    content_changed: usize,
    outside_redactions: usize,
}

#[allow(clippy::too_many_arguments)]
fn compare_pages(
    before: &Side,
    after: &Side,
    pairs: &Pairs,
    base_kept: bool,
    layers_changed: bool,
    budget: &mut Budget,
    rasters: &mut Rasters,
    inspection: &mut Inspection,
) -> Result<(), InspectError> {
    let mut counts = PageCounts::default();
    for &(old, new) in pairs {
        budget.time().map_err(|fail| fail.of(Role::Edited))?;
        let (Some(b), Some(e)) = (before.structure.pages.get(old), after.structure.pages.get(new)) else { continue };
        let drawn_differently = b.content != e.content || b.resources != e.resources || b.other != e.other || layers_changed;
        // The same content, resources and page entries draw the same glyphs and images.
        let pairing = if drawn_differently {
            glyphs::pair(&b.glyphs, &e.glyphs, budget).map_err(|fail| fail.of(Role::Edited))?
        } else {
            glyphs::Pairing { removed: Vec::new(), added: Vec::new() }
        };
        let lost_text = !pairing.removed.is_empty();
        let gained_text = !pairing.added.is_empty();
        let mut lost_image = images_missing(&b.images, &e.images);
        let mut gained_image = images_missing(&e.images, &b.images);
        if base_kept {
            counts.content_changed += usize::from(drawn_differently);
        } else if drawn_differently {
            let (base_raster, edited_raster) = rasters.get(before, after, (old, new), budget)?;
            let verdict = visual::redaction_boxes(&base_raster, &edited_raster);
            let boxes: Vec<[f32; 4]> = verdict.boxes.iter().map(|rect| user_rect(&edited_raster, *rect)).collect();
            let mut areas = Areas::new(&boxes);
            // Every removed glyph by its centre, and every image at its own resolution.
            let glyphs_inside = pairing.removed.iter().all(|&index| {
                b.glyphs
                    .glyph(index)
                    .is_some_and(|(_, rect)| areas.take_point(f64::from(rect[0] + rect[2]) / 2.0, f64::from(rect[1] + rect[3]) / 2.0))
            });
            let images = images::compare(&b.image_uses, &e.image_uses, (&before.images, &after.images), &mut areas, budget)
                .map_err(|fail| fail.of(Role::Edited))?;
            lost_image = images.removed;
            gained_image = images.added;
            if verdict.outside || !glyphs_inside || images.outside {
                counts.outside_redactions += 1;
            } else {
                let page = u32::try_from(new.saturating_add(1)).unwrap_or(u32::MAX);
                // An area that explains no removal is not a redaction.
                for (area, used) in boxes.into_iter().zip(areas.used) {
                    if !used {
                        continue;
                    }
                    if inspection.redaction_areas.len() >= MAX_REDACTION_AREAS {
                        return Err(InspectError::TooComplex);
                    }
                    inspection.redaction_areas.push((page, area));
                }
            }
        }
        counts.text_removed += usize::from(lost_text);
        counts.text_added += usize::from(gained_text);
        counts.images_removed += usize::from(lost_image);
        counts.images_added += usize::from(gained_image);
        counts.content_removed += usize::from(lost_text || lost_image);
    }
    inspection.text = TextChanges { pages_with_text_removed: count(counts.text_removed), pages_with_text_added: count(counts.text_added) };
    inspection.content = ContentChanges {
        pages_with_images_removed: count(counts.images_removed),
        pages_with_images_added: count(counts.images_added),
        pages_with_content_removed: count(counts.content_removed),
        pages_with_content_changed: count(counts.content_changed),
        pages_changed_outside_redactions: count(counts.outside_redactions),
    };
    Ok(())
}

/// How far (PDF points) a removed glyph's centre, or a changed image pixel's, may lie outside a
/// redaction area.
pub(crate) const AREA_TOLERANCE: f32 = 1.0;

/// A box of render pixels in the page's user space (PDF points), widened by its anti-aliased edge.
fn user_rect(raster: &UprightRaster, rect: visual::PixelRect) -> [f32; 4] {
    let scale = if raster.scale > 0.0 { raster.scale } else { 1.0 };
    let x = |px: usize| raster.crop[0] + px as f32 / scale;
    let y = |py: usize| raster.crop[1] + raster.height_pt - py as f32 / scale;
    let (x0, x1) = (rect.x0.saturating_sub(1), rect.x1.saturating_add(1).min(raster.width as usize));
    let (top, bottom) = (rect.y0.saturating_sub(1), rect.y1.saturating_add(1).min(raster.height as usize));
    [x(x0), y(bottom), x(x1), y(top)]
}

fn fields_changed(before: &Structure, after: &Structure) -> u32 {
    let names: BTreeSet<&String> = before.fields.keys().chain(after.fields.keys()).collect();
    let fields = names.into_iter().filter(|name| before.fields.get(*name) != after.fields.get(*name)).count();
    count(fields).saturating_add(u32::from(before.form != after.form))
}

fn add_kind(changes: &mut AnnotationChanges, subtype: &[u8]) {
    let slot = match subtype {
        b"Stamp" => &mut changes.stamps,
        b"Ink" | b"Line" | b"Square" | b"Circle" | b"Polygon" | b"PolyLine" => &mut changes.drawings,
        b"Text" | b"FreeText" | b"Highlight" | b"Underline" | b"StrikeOut" | b"Squiggly" | b"Caret" | b"FileAttachment" | b"Sound" => {
            &mut changes.comments
        }
        b"Redact" => &mut changes.redaction_marks,
        _ => &mut changes.other,
    };
    *slot = slot.saturating_add(1);
}

/// What the annotation comparison needs besides the annotations.
struct AnnotationContext<'s> {
    before: &'s Side,
    after: &'s Side,
    base_kept: bool,
}

impl AnnotationContext<'_> {
    /// A base annotation gone from the edited file: an applied redaction when it is a `Redact`
    /// mark of a page present in a rewrite and its whole area is now uniformly filled.
    fn gone(
        &self,
        annot: &Annot,
        pair: Option<(usize, usize)>,
        budget: &mut Budget,
        rasters: &mut Rasters,
        changes: &mut AnnotationChanges,
    ) -> Result<(), InspectError> {
        if !self.base_kept
            && annot.subtype == b"Redact"
            && !annot.areas.is_empty()
            && let Some(pair) = pair
        {
            let (base_raster, edited_raster) = rasters.get(self.before, self.after, pair, budget)?;
            if annot.areas.iter().all(|area| visual::area_filled(&base_raster, &edited_raster, *area)) {
                changes.redactions_applied = changes.redactions_applied.saturating_add(1);
                return Ok(());
            }
        }
        changes.removed = changes.removed.saturating_add(1);
        Ok(())
    }
}

fn compare_annotations(
    before: &Side,
    after: &Side,
    pairs: &Pairs,
    base_kept: bool,
    budget: &mut Budget,
    rasters: &mut Rasters,
) -> Result<AnnotationChanges, InspectError> {
    let context = AnnotationContext { before, after, base_kept };
    let mut changes = AnnotationChanges::default();
    let to_edited: HashMap<usize, usize> = pairs.iter().copied().collect();
    // Appended updates keep object numbers: their annotations are matched by identity.
    let mut edited_by_id: BTreeMap<ObjRef, (usize, &Annot)> = BTreeMap::new();
    // Per edited page, the annotations matched by content instead: all of them in a rewrite
    // (which renumbers objects), the direct dictionaries in an appended update.
    let mut by_content: BTreeMap<usize, Vec<&Annot>> = BTreeMap::new();
    for (index, page) in after.structure.pages.iter().enumerate() {
        for annot in page.annots.iter().filter(|annot| annot.tracked) {
            match annot.id {
                Some(id) if base_kept => {
                    edited_by_id.insert(id, (index, annot));
                }
                _ => by_content.entry(index).or_default().push(annot),
            }
        }
    }
    let mut seen: BTreeSet<ObjRef> = BTreeSet::new();
    for (old, page) in before.structure.pages.iter().enumerate() {
        let new = to_edited.get(&old).copied();
        let pair = new.map(|new| (old, new));
        for annot in page.annots.iter().filter(|annot| annot.tracked) {
            budget.tick().map_err(|fail| fail.of(Role::Edited))?;
            if base_kept && let Some(id) = annot.id {
                match edited_by_id.get(&id) {
                    Some((index, other)) => {
                        seen.insert(id);
                        if other.digest != annot.digest || Some(*index) != new {
                            changes.changed = changes.changed.saturating_add(1);
                        }
                    }
                    None => context.gone(annot, pair, budget, rasters, &mut changes)?,
                }
                continue;
            }
            let found = new.and_then(|new| {
                let list = by_content.get_mut(&new)?;
                let position = list.iter().position(|other| other.digest == annot.digest)?;
                Some(list.remove(position))
            });
            if found.is_none() {
                context.gone(annot, pair, budget, rasters, &mut changes)?;
            }
        }
    }
    for (id, (_, annot)) in &edited_by_id {
        if !seen.contains(id) {
            add_kind(&mut changes, &annot.subtype);
        }
    }
    for annot in by_content.values().flatten() {
        add_kind(&mut changes, &annot.subtype);
    }
    Ok(changes)
}

#[cfg(test)]
mod tests;
