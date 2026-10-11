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
//! - hayro (through `pdfcraft_render::content`) only interprets page content (each glyph, image
//!   and form it draws, and where: one interpretation, the one the render shows) and renders, and
//!   must agree with `pdfcraft_cos` on the page count and each page's annotation count; a glyph
//!   named neither by its text nor by its font and glyph, an image hayro draws that
//!   `pdfcraft_cos` does not read as an image, an interpreter warning or a rebuilt cross-reference
//!   table is an error;
//! - hayro interprets every optional-content layer as visible, whatever the document's
//!   configuration says: which layers show is each viewer's choice (a Layers panel shows hidden
//!   ones), and readers disagree on what marked content hides, so text, images and pixels are
//!   compared in every layer; each glyph, image and form also carries the optional content it is
//!   inside and whether it shows by default, evaluated here against `/OCProperties` (not as
//!   hayro would), and optional content that cannot be evaluated so that every reader agrees
//!   (a group named by another tag, missing from `/OCGs`, automatic states, no `/OCProperties`,
//!   marked content unbalanced around it, …) is an error;
//! - hayro reads every content stream it interprets to its end: one it stops reading early (an
//!   operator after which it gives up, data it cannot parse) or cannot decode is an error, since
//!   what follows would be checked by nothing; and it draws everything the content draws as
//!   `pdfcraft_cos` reads it (every image, directly, in forms and in tiling patterns' cells, and
//!   every form): anything it leaves undrawn is an error;
//! - the two read one document, checked: every object hayro reads (opening the file and drawing
//!   each page) must be where `pdfcraft_cos`'s cross-reference data puts it and pass the same
//!   checks, every object must parse to the same value in both (each dictionary entry and array
//!   item: no value any fact rests on is read by one parser alone), every stream's data must be
//!   the same bytes, each page must be the same object with the same inherited boxes and
//!   rotation, and
//!   its content as hayro decoded and joined it must be what `pdfcraft_cos` decoded; any
//!   difference is an error; so is a content stream hayro interprets (a form, a pattern's cell, a
//!   Type 3 glyph procedure) that it decoded to other data than `pdfcraft_cos` does; a stream
//!   whose `/Length` is not its data's length is read only in an object of the base revision
//!   (the base itself, or the base's bytes an appended update keeps; never an object a rewrite
//!   or an update writes, since the editor's writer declares every length exactly) and only
//!   where hayro read the same data;
//! - streams are compared by their decoded data only where hayro decoded them and the readers
//!   agree, everywhere else by their encoded bytes and encoding (see `structure`);
//! - an appended update must leave every base page's content and resources as they were (edit
//!   mode never writes page content there); a rewrite is accepted only as an applied redaction,
//!   checked visually (every changed pixel lies inside a uniformly filled black rectangle), glyph
//!   by glyph (a glyph is its text and its place; every removed glyph lies under a box, and none
//!   is added), image by image at each image's own resolution (pixels outside the boxes are
//!   the base's, or within requantisation of them for a JPEG), and content stream by content
//!   stream: forms drawn more than once, Type 3 fonts with everything their glyph procedures
//!   draw, and annotation appearances (a form or appearance that changed must lie wholly under
//!   the boxes; a Type 3 font may not change); a form drawn exactly once is part of its page's
//!   glyphs, images and pixels, and judged by them;
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
mod outline;
mod structure;
mod visual;

use images::{Areas, Images};
use structure::{Annot, Digest, Glyphs, PageFacts, Structure};

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
    pub outlines: OutlineChanges,
    pub destinations: DestinationChanges,
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
    /// Pages present in both files out of their order: the fewest of them that, moved, give the
    /// edited file's order, whether they changed or not (a page paired by its content counts as
    /// any other).
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
    /// (by its encoded data and encoding) changed, or that moved to another page. (A rewrite
    /// reports these as removed and added.)
    pub changed: u32,
    /// Rewrites: base `Redact` marks gone from a page where their area is now uniformly filled.
    pub redactions_applied: u32,
}

/// Among pages present in both files, glyph by glyph: a glyph is its name and its place (its em
/// box, within half a point on every side), and glyphs drawn as spaces count like any other. A
/// glyph's name is its text, or for a glyph without a Unicode value its font (the font
/// dictionary, by its content) and its glyph ID in the font. Glyphs in every optional-content
/// layer count, shown by default or not. Pages where a base glyph has no edited glyph with the
/// same name at the same place (text removed); pages where an edited glyph has no base glyph with
/// the same name at the same place (text added: moved or new text). A glyph's identity includes
/// its optional-content membership: the groups and membership dictionaries it is inside and
/// whether it shows by default, so a glyph moved into or out of a layer is removed and added. A
/// redaction removes text and adds none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextChanges {
    pub pages_with_text_removed: u32,
    pub pages_with_text_added: u32,
}

/// Among pages present in both files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentChanges {
    /// Pages that lost an image (drawn directly, inside a form XObject, or inline; in any
    /// optional-content layer). Appended updates compare images by a hash of each one's encoded
    /// stream (a cleared image is a new one). Rewrites compare each image placement where hayro's interpreter drew it (also in soft
    /// masks' groups; an image in a tiling pattern's cell by its hash alone): a base image gone,
    /// or replaced at its place by one whose pixels differ under a redaction area (an image
    /// redacted in place).
    pub pages_with_images_removed: u32,
    /// Pages that gained an image. In rewrites, an image redacted in place (the same size and
    /// meaning at the same place, pixels changed only under the redaction areas) is not one.
    pub pages_with_images_added: u32,
    /// Pages that lost text or an image, each page once.
    pub pages_with_content_removed: u32,
    /// Appended updates only (0 for rewrites): pages whose content streams, resources or other
    /// page-dictionary entries (all but `/Rotate` and `/Annots`) differ from the base page's, or
    /// every page present in both when the document's layer configuration changed. A content
    /// stream hayro decoded (a form, a pattern's cell, a Type 3 glyph procedure it drew) is
    /// compared by its decoded data: one written again with another encoding is not a change;
    /// every other stream by its encoded data.
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
    ///   or removed, unless it lay wholly under the boxes. Each image is placed where hayro's
    ///   interpreter drew it, and all of it is compared, whatever a clip hides;
    /// - an image drawn in a tiling pattern's cell added, removed or changed at all;
    /// - a form XObject drawn (directly, in forms or tiling patterns' cells, or as a soft mask's
    ///   group; compared by its dictionary and data, not its resources, and by its decoded data
    ///   where hayro decoded it, so not by its encoding) added, removed or changed, unless its
    ///   `/BBox` lies wholly under the boxes everywhere hayro drew it on the page (never for a
    ///   form drawn in a pattern's cell, or one hayro did not draw). A form drawn exactly once in
    ///   the file (on one page, at one place, directly or in forms) is not compared this way: what
    ///   it draws is part of its page's glyphs, images and pixels, compared as above;
    /// - a Type 3 font in the resources the content is drawn with added, removed or changed: its
    ///   dictionary, its glyph procedures, and every resource they name (forms, images, nested
    ///   Type 3 fonts, …, as they are looked up when the glyphs are drawn), recursively;
    /// - an annotation whose appearance alone changed (by its encoded data: no second reader
    ///   decodes it), unless its `/Rect`, before and after, lies wholly under the boxes;
    /// - every page, when the document's layer configuration (`/OCProperties`) changed: which
    ///   layers a viewer shows by default is not a redaction;
    /// - a page drawn with something hayro may render otherwise than other viewers (a blend mode
    ///   other than `/Normal`, a transfer function, a knockout group), on either side, so that a
    ///   change hayro's pixels do not show may show in another viewer;
    /// - a glyph, image or form in other optional content than at its place in the base, or shown
    ///   otherwise by default (R6b-29), whatever the boxes cover;
    /// - where content is in optional content: the page as it shows by default, or with every
    ///   layer off, changed other than under the boxes (content moved into or out of a layer,
    ///   or covered or uncovered by a layer's content, which the render of every layer cannot
    ///   tell).
    pub pages_changed_outside_redactions: u32,
}

/// The document outline (bookmarks), entry by entry. Entries are read depth first and paired in
/// that order by their depth and title; an entry whose title alone changed pairs with the base
/// entry at its place among the unpaired ones. Two paired entries must agree in everything else
/// (colour, style, any action other than going to a destination in the file), an added entry may
/// have no such action and must go to one of the file's pages, a named destination or nowhere,
/// and the outline's root dictionary may not change, or the inspection fails: no fact here
/// reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutlineChanges {
    /// Paired entries whose title differs.
    pub titles_changed: u32,
    /// Paired entries whose destination changed by following its page: the same page, at the
    /// same view, now at another page number (a page deleted or moved before it renumbers it),
    /// or no destination any more where its page was deleted (an editor drops those). Any other
    /// change of a destination (another page, another view, a new destination, a named or
    /// external one) fails the inspection.
    pub destinations_changed: u32,
    /// Edited entries paired with no base entry.
    pub added: u32,
    /// Base entries paired with no edited entry.
    pub removed: u32,
}

/// Named destinations and links' destinations that followed their page (R6b-32), as outline
/// destinations do (see [`OutlineChanges::destinations_changed`]): the same page, at the same
/// view, now at another page number (a page deleted or moved before it), or, where its page was
/// deleted, going nowhere (no destination, or a page reference that names no page of the file)
/// or, for a named destination, the name gone. A host may accept these as part of a page delete
/// or move. Any other change of a destination fails the inspection: another page, another view,
/// a destination gained, a named destination added, removed (its page still there) or retargeted.
/// A name tree that viewers could resolve differently (keys out of order or named twice,
/// `/Limits` that do not bound a node) fails it too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DestinationChanges {
    /// Named destinations (the catalog's `/Dests`, the `/Names /Dests` tree) whose destination
    /// followed its page.
    pub named: u32,
    /// Link annotations, on pages present in both files, whose destination (`/Dest`, or a plain
    /// go-to action) followed its page. Their annotation is otherwise unchanged: such a link is
    /// not counted in `annotations.changed`.
    pub links: u32,
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
    /// The steps pairing changed pages by similarity may take (a feature counted, a shared
    /// feature looked up for a page, a candidate pair weighed), in all.
    pub pairing: u64,
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
            pairing: 1 << 25,
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
    pairing_left: u64,
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
            pairing_left: limits.pairing,
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

    /// A step of pairing changed pages (see [`Limits::pairing`]).
    pub fn pairing(&mut self) -> Result<(), Fail> {
        self.pairing_left = self.pairing_left.checked_sub(1).ok_or(Fail::TooComplex)?;
        self.tick()
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
    let edited_fail = |fail: Fail| fail.of(Role::Edited);
    // Outline entries pair by title, not by page: where their destinations went says, between
    // changed pages equally alike, which went where.
    let outline_pairs = outline::pair(&before.structure.outline, &after.structure.outline, &mut budget).map_err(edited_fail)?;
    let votes = Votes::new(&before.structure, &after.structure, &outline_pairs);
    let pairs = match_pages(&before.structure.pages, &after.structure.pages, &mut inspection.pages, &votes, &mut budget).map_err(edited_fail)?;
    // Destinations are judged through the pages' pairing: one may follow its page, nothing more.
    let followed: BTreeMap<usize, usize> = pairs.iter().copied().collect();
    if before.structure.outline_root != after.structure.outline_root {
        return Err(InspectError::Edited("the outline's root dictionary changed, which this comparison does not report".into()));
    }
    inspection.outlines =
        outline::compare(&before.structure.outline, &after.structure.outline, outline_pairs, &followed, &mut budget).map_err(edited_fail)?;
    inspection.destinations.named =
        outline::named(&before.structure.named_destinations, &after.structure.named_destinations, &followed, &mut budget).map_err(edited_fail)?;
    // The document's layer configuration decides what every page shows.
    let layers_changed = before.structure.layers != after.structure.layers;
    inspection.form_fields_changed = fields_changed(&before.structure, &after.structure);
    let mut rasters = Rasters::default();
    compare_pages(&before, &after, &pairs, base_kept, layers_changed, &mut budget, &mut rasters, &mut inspection)?;
    inspection.redaction_areas.sort_by(|(page_a, a), (page_b, b)| page_a.cmp(page_b).then(b[3].total_cmp(&a[3])).then(a[0].total_cmp(&b[0])));
    (inspection.annotations, inspection.destinations.links) = compare_annotations(&before, &after, &pairs, base_kept, &mut budget, &mut rasters)?;
    Ok(inspection)
}

/// Read one file. `against` is the base (bytes, structure, whether `bytes` starts with it) when
/// reading the edited file: its cross-reference chain is checked against the base's. `placements`
/// keeps where each image and form was drawn, and takes out the forms drawn once (a rewrite's
/// page rules judge them).
fn read_side(bytes: &[u8], against: Option<(&[u8], &Structure, bool)>, placements: bool, budget: &mut Budget) -> Result<Side, Fail> {
    let shared = Arc::new(bytes.to_vec());
    let doc = match catch_unwind(AssertUnwindSafe(|| Document::open(shared.clone()))) {
        Ok(Ok(doc)) => doc,
        Ok(Err(CosError::NeedsPassword | CosError::WrongPassword | CosError::Security(_))) => return Err(Fail::Encrypted),
        // A file that names an encryption dictionary anywhere (an earlier revision's, which the
        // newest trailer dropped) is refused as encrypted: that is likely why it cannot be read.
        Ok(Err(_)) | Err(_) if names_encryption(bytes) => return Err(Fail::Encrypted),
        Ok(Err(error)) => return Err(Fail::Read(error.to_string())),
        Err(_) => return Err(Fail::Read("the file could not be parsed".into())),
    };
    if doc.security().is_some() || doc.trailer().contains(b"Encrypt") {
        return Err(Fail::Encrypted);
    }
    if let Some(repair) = doc.repair_log().first() {
        return Err(Fail::Read(format!("the file is damaged: {repair}")));
    }
    // Encryption in any revision, whatever the newest trailer says: the readers apply security
    // handlers, crypt filters (`/StmF`, `/StrF`, `/Identity`), `/EncryptMetadata` and the empty
    // password each their own way, and an update dropping `/Encrypt` leaves the older objects
    // encrypted for one reader and not for the other. No encrypted file is compared.
    let sections = structure::sections(&doc)?;
    if sections.iter().any(|section| section.encrypted) {
        return Err(Fail::Encrypted);
    }
    let hayro = match ContentDocument::open(shared.clone(), budget.limits.page_work) {
        Ok(hayro) => hayro,
        Err(ContentOpenError::Encrypted) => return Err(Fail::Encrypted),
        Err(ContentOpenError::Invalid) => return Err(Fail::Read("the file could not be parsed for its content".into())),
    };
    if hayro.page_count() > MAX_PAGES {
        return Err(Fail::TooComplex);
    }
    // hayro reads every page first: how `pdfcraft_cos`'s structure keys a content stream depends
    // on whether hayro decoded it (see `structure`). What it decoded is kept as digests only.
    let mut contents = Vec::with_capacity(hayro.page_count());
    let mut decoded: HashMap<ObjRef, Digest> = HashMap::new();
    for index in 0..hayro.page_count() {
        budget.time()?;
        let before = hayro.work_spent();
        let content = hayro.content(index);
        budget.work(&hayro, before)?;
        let mut content = content.map_err(content_failure)?;
        for ((num, generation), data) in std::mem::take(&mut content.resolved.content) {
            budget.hashed(data.len())?;
            let r = ObjRef::new(num, generation);
            let digest = structure::sha256(&[&data]);
            if *decoded.entry(r).or_insert(digest) != digest {
                return Err(Fail::Read(format!("readers disagree: hayro decoded content stream {r} two ways")));
            }
        }
        let page_data = content.decoded.take().unwrap_or_default();
        budget.hashed(page_data.len())?;
        budget.glyphs(content.glyphs.len().saturating_add(content.glyphs.chars.len()))?;
        contents.push((content, structure::sha256(&[&page_data])));
    }
    // The bytes this file keeps of the base: a /Length that disagrees with `endstream` is
    // tolerated only in the base's own objects (R6b-30).
    let kept_until = match against {
        None => usize::MAX,
        Some((base_bytes, _, true)) => base_bytes.len(),
        Some((_, _, false)) => 0,
    };
    let (structure, mut known) = structure::read(&doc, budget, Arc::new(decoded), sections, kept_until)?;
    if let Some((base_bytes, base, base_kept)) = against {
        structure::check_chain(&structure.sections, base_bytes.len(), &base.sections, base_kept)?;
    }
    // What hayro read opening the file (catalog, page tree, pages) is what `pdfcraft_cos` reads.
    known.agree(&doc, budget, hayro.opened())?;
    if hayro.page_count() != structure.pages.len() {
        return Err(Fail::Read(format!("readers disagree on the page count ({} or {})", structure.pages.len(), hayro.page_count())));
    }
    let mut side = Side { structure, hayro, images: Images::new(shared) };
    for (index, (page, (content, page_data))) in side.structure.pages.iter_mut().zip(contents).enumerate() {
        budget.time()?;
        let number = index.saturating_add(1);
        if side.hayro.annotation_count(index) != Some(page.annots_in_array) {
            return Err(Fail::Read(format!("readers disagree on the annotations of page {number}")));
        }
        page.size = side.hayro.upright_size(index).ok_or_else(|| Fail::Read(format!("page {number} could not be read")))?;
        // The page's boxes and rotation, inherited through the page tree, are what hayro took:
        // hayro skips a value it cannot read and inherits another (or a default).
        let (media, crop, rotation) = side.hayro.page_geometry(index).ok_or_else(|| Fail::Read(format!("page {number} could not be read")))?;
        // hayro reads each number as a 32-bit float.
        let same = |ours: &[f64; 4], theirs: &[f64; 4]| ours.iter().zip(theirs).all(|(a, b)| f64::from(*a as f32) == *b);
        if !same(&page.boxes.0, &media) || !same(page.boxes.1.as_ref().unwrap_or(&page.boxes.0), &crop) || i64::from(rotation) != page.rotation {
            return Err(Fail::Read(format!("readers disagree on the boxes or rotation page {number} inherits")));
        }
        budget.content(page.interpreted)?;
        // The page hayro drew is this page, its decoded content this content, and every object
        // it read to draw it the object `pdfcraft_cos` reads.
        if content.page != Some((page.object.num, page.object.generation)) {
            return Err(Fail::Read(format!("readers disagree: page {number} is another object to hayro")));
        }
        if page_data != page.content {
            return Err(Fail::Read(format!("readers disagree on the content of page {number}")));
        }
        known.agree(&doc, budget, &content.resolved)?;
        // Every image and form the content draws (as `pdfcraft_cos` reads it) is one hayro drew.
        structure::check_drawn(page, &content, number)?;
        // A glyph without text is named by its font's content, which a rewrite does not renumber.
        let mut unnamed = HashMap::with_capacity(content.glyphs.unnamed.len());
        for (glyph, (num, generation), id) in &content.glyphs.unnamed {
            let font = known.font_digest(&doc, budget, ObjRef::new(*num, *generation))?;
            unnamed.insert(*glyph, (font, *id));
        }
        // Optional content (R6b-29): every mark the page met is evaluated against the document's
        // configuration (or the page refused), and each glyph, image and form carries the
        // membership it was drawn in.
        let (hidden, objects) = known.hidden_layers(&doc, budget, &content.layer_marks)?;
        if !objects.is_empty() && content.layers_unbalanced {
            return Err(Fail::Read(format!(
                "page {number}: marked content is not balanced in a content stream, so which content is in which layer is each reader's guess"
            )));
        }
        let memberships = content.layers.iter().map(|marks| known.layer(&doc, budget, marks)).collect::<Result<Vec<_>, _>>()?;
        let membership = |index: Option<u32>| index.and_then(|index| memberships.get(index as usize).copied().flatten());
        page.hidden_layers = hidden;
        page.layer_objects = objects;
        let layers: HashMap<u32, structure::LayerFact> =
            content.glyphs.layers.iter().filter_map(|(glyph, index)| membership(Some(*index)).map(|fact| (*glyph, fact))).collect();
        // Where each image is drawn is where hayro's interpretation (the one the render check
        // sees) drew it; `pdfcraft_cos` says which image it is.
        if placements {
            page.image_uses = known.image_uses(&doc, budget, &content.images)?;
            for (used, draw) in page.image_uses.iter_mut().zip(&content.images) {
                used.layer = membership(draw.layer);
            }
            page.form_layers = content.forms.iter().map(|form| membership(form.layer)).collect();
            page.form_draws = content.forms;
        }
        page.glyphs = Glyphs { page: content.glyphs, unnamed, layers };
        page.fingerprint = structure::fingerprint(page);
    }
    // Every object `pdfcraft_cos` read (pages, resources, annotations, form fields, the outline,
    // the catalog's entries, …) is one hayro reads at the same place, with the same data: no
    // object any fact rests on is identified by one reader alone.
    let read = known.objects_read(&doc);
    for _ in &read {
        budget.object()?;
    }
    let (resolved, unread) = side.hayro.read_objects(&read);
    if let Some((num, generation)) =
        unread.into_iter().find(|(num, _)| doc.try_get(*num).is_ok_and(|object| !matches!(*object, pdfcraft_cos::Object::Null)))
    {
        return Err(Fail::Read(format!("readers disagree: hayro cannot read object {num} {generation} R")));
    }
    known.agree(&doc, budget, &resolved)?;
    // And parses to the same value: every dictionary entry and array item any fact rests on.
    known.agree_shapes(&doc, budget, &read, &side.hayro.object_shapes(&read))?;
    // Every content stream hayro decoded is one `pdfcraft_cos` decodes alike, and every stream
    // whose /Length is wrong one hayro read as the same data.
    known.agree_decoded(&doc, budget)?;
    if let Some(stream) = known.unconfirmed_length() {
        return Err(Fail::Read(format!("stream {stream} does not have the length it declares")));
    }
    if placements {
        structure::retain_shared_forms(&mut side.structure.pages);
    }
    drop(doc);
    if side.hayro.repaired() {
        return Err(Fail::Read("the file's cross-reference data is damaged".into()));
    }
    Ok(side)
}

/// Whether `bytes` hold the name `/Encrypt` (not `/EncryptMetadata`).
fn names_encryption(bytes: &[u8]) -> bool {
    let name = b"/Encrypt";
    bytes
        .windows(name.len())
        .enumerate()
        .any(|(at, window)| window == name && bytes.get(at.saturating_add(name.len())).is_none_or(|next| !next.is_ascii_alphanumeric()))
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

/// A page's content as a multiset of features, for pairing changed pages by similarity: each
/// glyph by its name and the 4-point cell of its centre, each image by its digest and the 4-point
/// cell of its placement's origin (where its placements are known, else by its digest alone).
/// The cells are coarse on purpose: a rewrite that sets kept text a little off still leaves most
/// glyphs in their cells. Only which pages pair rests on this; the comparison that follows is
/// exact.
fn features(page: &PageFacts, budget: &mut Budget) -> Result<HashMap<u64, u32>, Fail> {
    use std::hash::{Hash, Hasher};
    let cell = |value: f64| (value / 4.0).floor() as i64;
    let mut out: HashMap<u64, u32> = HashMap::new();
    let mut add = |key: u64| {
        let slot = out.entry(key).or_default();
        *slot = slot.saturating_add(1);
    };
    for index in 0..page.glyphs.len() {
        budget.pairing()?;
        let Some((name, rect)) = page.glyphs.glyph(index) else { return Err(Fail::Read("a glyph went missing".into())) };
        let mut hasher = std::hash::DefaultHasher::new();
        b'g'.hash(&mut hasher);
        match name {
            structure::GlyphName::Text(chars) => chars.hash(&mut hasher),
            structure::GlyphName::Font(font, id) => (font, id).hash(&mut hasher),
        }
        cell((f64::from(rect[0]) + f64::from(rect[2])) / 2.0).hash(&mut hasher);
        cell((f64::from(rect[1]) + f64::from(rect[3])) / 2.0).hash(&mut hasher);
        add(hasher.finish());
    }
    if page.image_uses.is_empty() {
        for digest in &page.images {
            budget.pairing()?;
            let mut hasher = std::hash::DefaultHasher::new();
            (b'i', digest).hash(&mut hasher);
            add(hasher.finish());
        }
    } else {
        for used in &page.image_uses {
            budget.pairing()?;
            let mut hasher = std::hash::DefaultHasher::new();
            (b'p', &used.digest, used.tiled).hash(&mut hasher);
            if used.matrix[4].is_finite() && used.matrix[5].is_finite() {
                (cell(used.matrix[4]), cell(used.matrix[5])).hash(&mut hasher);
            }
            add(hasher.finish());
        }
    }
    Ok(out)
}

/// A feature shared by more of the changed base pages than this tells none of them apart (a
/// running header): it does not count toward any page's similarity.
const MAX_SHARED_FEATURE: usize = 16;

/// The most candidate pairs of changed pages weighed: each edited page shares features with at
/// most [`MAX_SHARED_FEATURE`] base pages a feature, so this is reached only by pages built to
/// look alike in groups.
const MAX_CANDIDATE_PAIRS: usize = 1 << 20;

/// The similarity of each changed base page to each changed edited page that shares content
/// with it: (score, base page, edited page, the edited page's features the base page lacks). The
/// score is the features the two share, less those the base page lacks (a redaction removes
/// content and adds none).
fn similarities(
    before: &[PageFacts],
    after: &[PageFacts],
    base_free: &[usize],
    edited_free: &[usize],
    budget: &mut Budget,
) -> Result<Vec<(i64, usize, usize, i64)>, Fail> {
    let mut index: HashMap<u64, Vec<(usize, u32)>> = HashMap::new();
    for &page in base_free {
        let Some(facts) = before.get(page) else { continue };
        for (key, count) in features(facts, budget)? {
            index.entry(key).or_default().push((page, count));
        }
    }
    let mut out = Vec::new();
    for &other in edited_free {
        let Some(facts) = after.get(other) else { continue };
        let found = features(facts, budget)?;
        let total: i64 = found.values().map(|count| i64::from(*count)).sum();
        let mut shared: BTreeMap<usize, i64> = BTreeMap::new();
        for (key, count) in &found {
            let Some(list) = index.get(key) else { continue };
            if list.len() > MAX_SHARED_FEATURE {
                continue;
            }
            for (page, base_count) in list {
                budget.pairing()?;
                *shared.entry(*page).or_default() += i64::from((*count).min(*base_count));
            }
        }
        for (page, common) in shared {
            let lacking = total.saturating_sub(common);
            out.push((common.saturating_sub(lacking), page, other, lacking));
        }
    }
    Ok(out)
}

/// What the documents' destinations say about where pages went: an outline entry (paired by its
/// title) or a named destination (by its name) going to base page `b` and now to edited page `e`
/// votes for that pair; one that went to `b` and now goes nowhere says `b` was deleted. Used
/// only to choose between changed pages equally alike (see [`match_pages`]): the pages are then
/// compared as paired, and every destination must have followed that pairing.
#[derive(Default)]
struct Votes {
    pairs: HashMap<(usize, usize), u32>,
    deleted: HashMap<usize, u32>,
}

impl Votes {
    fn new(before: &Structure, after: &Structure, outline_pairs: &[(usize, usize)]) -> Self {
        let mut votes = Self::default();
        for &(old, new) in outline_pairs {
            if let (Some(a), Some(b)) = (before.outline.get(old), after.outline.get(new)) {
                votes.vote(&a.destination, b.destination.clone());
            }
        }
        for (name, destination) in &before.named_destinations {
            votes.vote(destination, after.named_destinations.get(name).cloned().unwrap_or(structure::Destination::None));
        }
        votes
    }

    fn vote(&mut self, before: &structure::Destination, after: structure::Destination) {
        use structure::Destination;
        let Destination::Page(page, _) = before else { return };
        let slot = match after {
            Destination::Page(other, _) => self.pairs.entry((*page, other)).or_default(),
            Destination::None | Destination::Unpaged(_) => self.deleted.entry(*page).or_default(),
            _ => return,
        };
        *slot = slot.saturating_add(1);
    }
}

/// Pair the pages: identical pages in order, then identical pages elsewhere (moved), then the
/// changed pages by content similarity (R6b-31: the glyphs and images they share, at their
/// places; see [`similarities`]), the most similar first; position decides only between equally
/// similar pages (and pairs pages that share nothing): a page between the same two pages paired
/// in order first, then the nearest. Before position, between equally similar pages and for
/// pages that share nothing, what the destinations say ([`Votes`]). What is left was inserted
/// or deleted.
fn match_pages(before: &[PageFacts], after: &[PageFacts], changes: &mut PageChanges, votes: &Votes, budget: &mut Budget) -> Result<Pairs, Fail> {
    let base_prints: Vec<Digest> = before.iter().map(|page| page.fingerprint).collect();
    let edited_prints: Vec<Digest> = after.iter().map(|page| page.fingerprint).collect();
    let anchors = in_order(&base_prints, &edited_prints);
    let mut pairs = anchors.clone();
    let used_base: BTreeSet<usize> = pairs.iter().map(|pair| pair.0).collect();
    let used_edited: BTreeSet<usize> = pairs.iter().map(|pair| pair.1).collect();
    let mut base_free: Vec<usize> = (0..before.len()).filter(|page| !used_base.contains(page)).collect();
    let mut edited_free: Vec<usize> = (0..after.len()).filter(|page| !used_edited.contains(page)).collect();
    base_free.retain(|&page| {
        let Some(position) = edited_free.iter().position(|&other| base_prints.get(page) == edited_prints.get(other)) else { return true };
        pairs.push((page, edited_free.remove(position)));
        false
    });
    // Which gap between pages paired in order a page lies in.
    let slot_of_base = |page: usize| anchors.partition_point(|anchor| anchor.0 < page);
    let slot_of_edited = |page: usize| anchors.partition_point(|anchor| anchor.1 < page);
    let said = |page: usize, other: usize| {
        (std::cmp::Reverse(votes.pairs.get(&(page, other)).copied().unwrap_or(0)), votes.deleted.get(&page).copied().unwrap_or(0))
    };
    let apart = |page: usize, other: usize| (said(page, other), slot_of_base(page).abs_diff(slot_of_edited(other)), page.abs_diff(other));
    let mut candidates = similarities(before, after, &base_free, &edited_free, budget)?;
    if candidates.len() > MAX_CANDIDATE_PAIRS {
        return Err(Fail::TooComplex);
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(apart(a.1, a.2).cmp(&apart(b.1, b.2))).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    candidates.dedup();
    let mut base_taken: BTreeSet<usize> = BTreeSet::new();
    let mut edited_taken: BTreeSet<usize> = BTreeSet::new();
    // Each candidate's score, by edited page and by base page, for telling pages apart.
    let mut by_edited: HashMap<usize, Vec<(usize, i64, i64)>> = HashMap::new();
    let mut by_base: HashMap<usize, Vec<(usize, i64, i64)>> = HashMap::new();
    for &(score, page, other, lacking) in &candidates {
        by_edited.entry(other).or_default().push((page, score, lacking));
        by_base.entry(page).or_default().push((other, score, lacking));
    }
    for &(score, page, other, lacking) in &candidates {
        budget.pairing()?;
        if base_taken.contains(&page) || edited_taken.contains(&other) {
            continue;
        }
        // The pairing must be certain: another page still free as alike as this one (within
        // a margin), lacking no more of the other's content (so that one could be a redaction
        // of the other as well), leaves which page became which to a guess, unless the
        // destinations say which (and are then checked against it).
        let margin = (score / 16).max(2);
        let said = votes.pairs.get(&(page, other)).copied().unwrap_or(0) > 0;
        let alike = |rival_score: i64, rival_lacking: i64| rival_score >= score.saturating_sub(margin) && rival_lacking <= lacking;
        let rival_base = by_edited.get(&other).into_iter().flatten().find(|(rival, rival_score, rival_lacking)| {
            *rival != page
                && !base_taken.contains(rival)
                && alike(*rival_score, *rival_lacking)
                && (!said || votes.pairs.contains_key(&(*rival, other)))
        });
        let rival_edited = by_base.get(&page).into_iter().flatten().find(|(rival, rival_score, rival_lacking)| {
            *rival != other
                && !edited_taken.contains(rival)
                && alike(*rival_score, *rival_lacking)
                && (!said || votes.pairs.contains_key(&(page, *rival)))
        });
        if let Some((rival, ..)) = rival_base {
            return Err(Fail::Read(format!(
                "the pages cannot be told apart: base pages {} and {} are as alike to edited page {}",
                page.saturating_add(1),
                rival.saturating_add(1),
                other.saturating_add(1)
            )));
        }
        if let Some((rival, ..)) = rival_edited {
            return Err(Fail::Read(format!(
                "the pages cannot be told apart: edited pages {} and {} are as alike to base page {}",
                other.saturating_add(1),
                rival.saturating_add(1),
                page.saturating_add(1)
            )));
        }
        base_taken.insert(page);
        edited_taken.insert(other);
        pairs.push((page, other));
    }
    base_free.retain(|page| !base_taken.contains(page));
    edited_free.retain(|page| !edited_taken.contains(page));
    // Pages that share nothing with any other: as the destinations went, then in order within
    // the same gap (pages a destination says were deleted last), then in order.
    let mut positional = Vec::new();
    let mut voted: Vec<(&(usize, usize), &u32)> = votes.pairs.iter().collect();
    voted.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (&(page, other), _) in voted {
        if base_free.contains(&page) && edited_free.contains(&other) {
            positional.push((page, other));
            base_free.retain(|free| *free != page);
            edited_free.retain(|free| *free != other);
        }
    }
    base_free.sort_by_key(|page| (votes.deleted.get(page).copied().unwrap_or(0), *page));
    for &page in &base_free {
        if let Some(position) = edited_free.iter().position(|&other| slot_of_edited(other) == slot_of_base(page)) {
            positional.push((page, edited_free.remove(position)));
        }
    }
    base_free.retain(|page| !positional.iter().any(|pair| pair.0 == *page));
    base_free.sort_unstable();
    let rest = base_free.len().min(edited_free.len());
    positional.extend(base_free.drain(..rest).zip(edited_free.drain(..rest)));
    pairs.extend(positional.iter().copied());
    *changes = PageChanges {
        before: count(before.len()),
        after: count(after.len()),
        rotated: count(pairs.iter().filter(|(old, new)| before.get(*old).map(|p| p.rotation) != after.get(*new).map(|p| p.rotation)).count()),
        inserted: count(edited_free.len()),
        deleted: count(base_free.len()),
        moved: 0,
    };
    pairs.sort_unstable();
    // Pages out of their order, whatever paired them (the same page elsewhere, or a changed page
    // by similarity): the fewest that, moved, give the edited order.
    let in_place = longest_increasing(&pairs.iter().map(|pair| pair.1).collect::<Vec<_>>());
    changes.moved = count(pairs.len().saturating_sub(in_place));
    Ok(pairs)
}

/// The length of the longest strictly increasing subsequence of `values`.
fn longest_increasing(values: &[usize]) -> usize {
    let mut tails: Vec<usize> = Vec::new();
    for &value in values {
        let at = tails.partition_point(|tail| *tail < value);
        match tails.get_mut(at) {
            Some(slot) => *slot = value,
            None => tails.push(value),
        }
    }
    tails.len()
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

/// Whether glyphs or forms of the base page are on the edited page in other optional content
/// (R6b-29): glyphs that pair by name and place but not with their membership (`pairing` paired
/// them with it), or more forms drawn in some membership than the base page draws there (a
/// redaction only removes forms). Images are compared with theirs (see `images`).
fn layers_changed_for(base: &PageFacts, edited: &PageFacts, pairing: &glyphs::Pairing, budget: &mut Budget) -> Result<bool, Fail> {
    if !(base.glyphs.layers.is_empty() && edited.glyphs.layers.is_empty()) {
        let loose = glyphs::pair_ignoring_layers(&base.glyphs, &edited.glyphs, budget)?;
        if loose.removed.len() < pairing.removed.len() {
            return Ok(true);
        }
    }
    let tally = |page: &PageFacts| {
        let mut counts: BTreeMap<Option<structure::LayerFact>, usize> = BTreeMap::new();
        for layer in &page.form_layers {
            let count = counts.entry(*layer).or_default();
            *count = count.saturating_add(1);
        }
        counts
    };
    let (in_base, in_edited) = (tally(base), tally(edited));
    Ok(in_edited.iter().any(|(layer, count)| *count > in_base.get(layer).copied().unwrap_or(0)))
}

/// The page rendered as a viewer shows it by default: without the content inside `hidden`
/// optional content (see [`PageFacts::hidden_layers`]).
fn render_hiding(side: &Side, index: usize, hidden: &[(i32, i32)], budget: &mut Budget) -> Result<UprightRaster, Fail> {
    render_with(side, index, hidden, budget)
}

fn render(side: &Side, index: usize, budget: &mut Budget) -> Result<UprightRaster, Fail> {
    render_with(side, index, &[], budget)
}

fn render_with(side: &Side, index: usize, hidden: &[(i32, i32)], budget: &mut Budget) -> Result<UprightRaster, Fail> {
    budget.time()?;
    let page = side.structure.pages.get(index).ok_or_else(|| Fail::Read("a page went missing".into()))?;
    if page.image_pixels > budget.limits.page_image_pixels || page.interpreted > budget.limits.page_render_content {
        return Err(Fail::TooComplex);
    }
    let side_px = u64::from(budget.limits.render_side);
    budget.pixels(side_px.saturating_mul(side_px))?;
    budget.content(page.interpreted)?;
    let before = side.hayro.work_spent();
    let raster = side.hayro.render_upright_hiding(index, budget.limits.render_side, hidden);
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
        } else {
            // Unchanged content and resources leave only the annotations to compare (nothing on
            // the page explains a change in them).
            let (boxes, mut outside) = if drawn_differently {
                let (base_raster, edited_raster) = rasters.get(before, after, (old, new), budget)?;
                let verdict = visual::redaction_boxes(&base_raster, &edited_raster);
                (verdict.boxes.iter().map(|rect| user_rect(&edited_raster, *rect)).collect(), verdict.outside)
            } else {
                (Vec::new(), false)
            };
            // R6b-29. What the page shows by default, where some content is in a layer that is
            // off, and with every layer off: the render of every layer cannot tell content moved
            // into or out of a layer, or covered by a layer's content, from what was there.
            if drawn_differently {
                let views = [(&b.hidden_layers, &e.hidden_layers), (&b.layer_objects, &e.layer_objects)];
                for (base_hidden, edited_hidden) in views {
                    if outside || (base_hidden.is_empty() && edited_hidden.is_empty()) {
                        continue;
                    }
                    let base_view = render_hiding(before, old, base_hidden, budget).map_err(|fail| fail.of(Role::Base))?;
                    let edited_view = render_hiding(after, new, edited_hidden, budget).map_err(|fail| fail.of(Role::Edited))?;
                    outside |= visual::redaction_boxes(&base_view, &edited_view).outside;
                }
            }
            // A glyph, image or form now in other optional content (or shown otherwise by
            // default) than it was: not a redaction, whatever the boxes cover.
            if drawn_differently {
                outside |= layers_changed_for(b, e, &pairing, budget).map_err(|fail| fail.of(Role::Edited))?;
            }
            let mut areas = Areas::new(&boxes);
            // Every removed glyph by its centre, and every image at its own resolution.
            let glyphs_inside = pairing.removed.iter().all(|&index| {
                b.glyphs
                    .glyph(index)
                    .is_some_and(|(_, rect)| areas.take_point(f64::from(rect[0] + rect[2]) / 2.0, f64::from(rect[1] + rect[3]) / 2.0))
            });
            if drawn_differently {
                let images = images::compare(&b.image_uses, &e.image_uses, (&before.images, &after.images), &mut areas, budget)
                    .map_err(|fail| fail.of(Role::Edited))?;
                lost_image = images.removed;
                gained_image = images.added;
                outside |= images.outside;
            }
            // Content streams: forms drawn more than once where hayro drew them, Type 3 fonts
            // (whose procedures draw unclipped, wherever the glyph is) and annotations'
            // appearances by their rectangle.
            outside |= !glyphs_inside
                || !forms_under(b, e, &mut areas, budget).map_err(|fail| fail.of(Role::Edited))?
                || b.type3 != e.type3
                || !appearances_under(b, e, &mut areas, budget).map_err(|fail| fail.of(Role::Edited))?
                // Which layers show by default changed: not a redaction, whatever the boxes.
                || layers_changed
                // Changed, and drawn with something other viewers may show otherwise than
                // hayro's pixels, on either side (R6b-32).
                || (drawn_differently && (b.render_risk.is_some() || e.render_risk.is_some()));
            if outside {
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

/// Whether the forms the base page draws and the edited page draws differ only under the areas:
/// each form (by its key, see [`PageFacts::forms`]) the one page draws more often than the other
/// must lie wholly under the areas, by its `/BBox` everywhere hayro drew it on that page. A form
/// hayro did not draw on the page (in a tiling pattern's cell) is never under them. Forms drawn
/// once in their file are not among them (see `structure::retain_shared_forms`).
fn forms_under(base: &PageFacts, edited: &PageFacts, areas: &mut Areas, budget: &mut Budget) -> Result<bool, Fail> {
    let tally = |page: &PageFacts| {
        let mut counts: BTreeMap<Digest, usize> = BTreeMap::new();
        for form in &page.forms {
            let count = counts.entry(form.key).or_default();
            *count = count.saturating_add(1);
        }
        counts
    };
    let (in_base, in_edited) = (tally(base), tally(edited));
    let keys: BTreeSet<&Digest> = in_base.keys().chain(in_edited.keys()).collect();
    for key in keys {
        budget.tick()?;
        let (from_base, from_edited) = (in_base.get(key).copied().unwrap_or(0), in_edited.get(key).copied().unwrap_or(0));
        if from_base == from_edited {
            continue;
        }
        let page = if from_base > from_edited { base } else { edited };
        let objects: BTreeSet<ObjRef> = page.forms.iter().filter(|form| &form.key == key).map(|form| form.obj).collect();
        let mut placed = false;
        for draw in page.form_draws.iter().filter(|draw| objects.contains(&ObjRef::new(draw.num, draw.generation))) {
            if draw.tiled || !areas.take_rect(&draw.rect) {
                return Ok(false);
            }
            placed = true;
        }
        if !placed {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether the annotations of the base page and the edited page that are alike but for their
/// appearance (compared as a stream: see `structure`) lie wholly under the areas, by `/Rect`.
fn appearances_under(base: &PageFacts, edited: &PageFacts, areas: &mut Areas, budget: &mut Budget) -> Result<bool, Fail> {
    let mut by_digest: HashMap<Digest, Vec<&Annot>> = HashMap::new();
    for annot in edited.annots.iter().filter(|annot| annot.tracked) {
        by_digest.entry(annot.digest).or_default().push(annot);
    }
    let mut changed: Vec<&Annot> = Vec::new();
    for annot in base.annots.iter().filter(|annot| annot.tracked) {
        budget.tick()?;
        if by_digest.get_mut(&annot.digest).and_then(Vec::pop).is_none() {
            changed.push(annot);
        }
    }
    // The edited annotations left, by their dictionaries without the appearance.
    let mut by_bare: HashMap<Digest, Vec<&Annot>> = HashMap::new();
    for annot in by_digest.into_values().flatten() {
        by_bare.entry(annot.bare).or_default().push(annot);
    }
    for annot in changed {
        budget.tick()?;
        let Some(other) = by_bare.get_mut(&annot.bare).and_then(Vec::pop) else { continue };
        for rect in [annot.rect, other.rect] {
            if !rect.is_some_and(|rect| areas.take_rect(&rect)) {
                return Ok(false);
            }
        }
    }
    Ok(true)
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

/// The annotations compared, and how many links' destinations followed their page.
fn compare_annotations(
    before: &Side,
    after: &Side,
    pairs: &Pairs,
    base_kept: bool,
    budget: &mut Budget,
    rasters: &mut Rasters,
) -> Result<(AnnotationChanges, u32), InspectError> {
    let context = AnnotationContext { before, after, base_kept };
    let mut changes = AnnotationChanges::default();
    let mut links = 0u32;
    let to_edited: HashMap<usize, usize> = pairs.iter().copied().collect();
    let pages: BTreeMap<usize, usize> = pairs.iter().copied().collect();
    // A link's destination may change only by following its page (R6b-32).
    let link_moved = |a: &Annot, b: &Annot| -> Result<bool, InspectError> {
        let none = structure::Destination::None;
        outline::followed(a.link.as_ref().unwrap_or(&none), b.link.as_ref().unwrap_or(&none), &pages)
            .map_err(|_| InspectError::Edited("a link's destination changed other than by following its page".into()))
    };
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
                        let moved = link_moved(annot, other)?;
                        if other.digest != annot.digest || Some(*index) != new {
                            changes.changed = changes.changed.saturating_add(1);
                        } else {
                            links = links.saturating_add(u32::from(moved));
                        }
                    }
                    None => context.gone(annot, pair, budget, rasters, &mut changes)?,
                }
                continue;
            }
            // The same dictionary, its destination (a link's) followed.
            let mut found = None;
            let mut retargeted = false;
            if let Some(list) = new.and_then(|new| by_content.get_mut(&new)) {
                for (position, other) in list.iter().enumerate() {
                    if other.digest != annot.digest {
                        continue;
                    }
                    match link_moved(annot, other) {
                        Ok(moved) => {
                            found = Some((position, moved));
                            break;
                        }
                        Err(_) => retargeted = true,
                    }
                }
                if let Some((position, moved)) = found {
                    list.remove(position);
                    links = links.saturating_add(u32::from(moved));
                }
            }
            if found.is_none() {
                if retargeted {
                    return Err(InspectError::Edited("a link's destination changed other than by following its page".into()));
                }
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
    Ok((changes, links))
}

#[cfg(test)]
mod agreement_tests;
#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod destination_tests;
#[cfg(test)]
mod layer_tests;
#[cfg(test)]
mod limit_tests;
#[cfg(test)]
mod placement_tests;
#[cfg(test)]
mod tests;
