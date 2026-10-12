//! Embed build (R6b-49, P6-32, P6-33, P6-35): a save's redaction marks applied on the host's
//! server, never in the browser. The embed build marks areas (`Redact` annotations in an
//! appended update) and never applies them; the host inspects the update, then calls
//! [`apply_marks`], which applies the marks to the edited document with the same redactor,
//! writes the result whole and canonically, and checks its own output with the inspector against
//! the edited document: the output is accepted only as exactly the marks applied, nothing else
//! changed. Any rewrite a browser uploads is refused by the host.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use pdfcraft_cos::{Dict, Document, ObjRef, Object, SaveOptions, Stream};

use super::structure::sha256;
use super::{Budget, InspectError, InspectLimits, Inspection, Limits, inspect, operators};

/// What applying a save's marks gives: the document to store, the inspector's account of it
/// against the edited document (see [`apply_marks`]), and what the redaction did.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The redacted document, written whole: every object `pdfcraft_cos`'s writer serialises,
    /// renumbered, in object streams, with a fresh file identifier; no byte of the edited file
    /// carried over as it was written, no comment, a `%PDF-x.y` header.
    pub bytes: Vec<u8>,
    /// `inspect(edited, bytes)`: a rewrite whose only change is the marks applied (checked, see
    /// [`apply_marks`]).
    pub inspection: Inspection,
    pub facts: AppliedFacts,
}

/// What a redaction did (counts; no content).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppliedFacts {
    /// The marks applied, and the pages they were on.
    pub marks: u32,
    pub pages: u32,
    /// One area per mark quadrilateral (or `/Rect`, without `/QuadPoints`): (page number,
    /// from 1; the area in the page's default user space, before `/Rotate`, without
    /// `/UserUnit`), in page order then the marks' order (P6-35). Each was filled black.
    pub areas: Vec<(u32, [f32; 4])>,
    pub glyphs: u32,
    pub images_removed: u32,
    pub images_cleared: u32,
    pub paths_removed: u32,
    pub paths_clipped: u32,
    pub forms_rewritten: u32,
    pub forms_removed: u32,
    /// Annotations removed under the marks, with their replies (`/IRT`).
    pub annotations_removed: u32,
    /// Form fields removed with a widget under a mark.
    pub form_fields_removed: u32,
    /// Structure elements and spans that lost text standing for what was removed.
    pub tags: u32,
    pub thumbnails: u32,
    pub resources_removed: u32,
    /// Document-level copies of the removed words (R6b-50): bookmark titles now "[Redacted]",
    /// document information entries removed, the XMP packet removed, named destinations renamed.
    pub bookmarks_cleared: u32,
    pub information_cleared: u32,
    pub metadata_removed: u32,
    pub destinations_renamed: u32,
}

/// Why marks could not be applied. The host keeps the edited file unsaved on every one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The edited file is not an appended update of the base that passes inspection.
    Inspect(InspectError),
    /// It holds no redaction mark.
    NothingToApply,
    /// The redactor refused (content it cannot place or verify under a mark).
    Refused(String),
    /// The output failed its own inspection: something besides the marks applied changed.
    SelfCheck(String),
    /// A work bound or the deadline was exceeded.
    TooComplex,
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Apply the redaction marks of `edited` (the base plus an appended update holding them) to
/// it. Bounded as [`inspect`] is: `limits.deadline` covers the whole call natively (both
/// inspections and the redaction between them); the redactor has a work bound of its own, and
/// every inspection its work bounds.
///
/// In order:
/// - `inspect(base, edited)` must succeed with `edited` an appended update of `base` holding at
///   least one mark;
/// - every mark of `edited` is applied (`pdfcraft_redact::apply_with`, every area filled black,
///   overlays ignored, document-level copies of the removed words cleared);
/// - the document is written whole: the file identifier made fresh, every content stream the
///   redaction wrote re-emitted from its operators as the content serializer writes them;
/// - the output is inspected against `edited`, and accepted only as a rewrite with every mark
///   applied (each of its areas filled) and nothing else changed: no page added, removed,
///   moved or rotated, nothing outside the redactions, no text or image added, no annotation
///   added, removed or changed but the marks, no form field changed, no outline, destination
///   or page-label change but the copies cleared, no document information change but entries
///   removed; every filled area within a point of a mark area; every content stream the output
///   brings exactly as the content serializer writes it, byte for byte.
pub fn apply_marks(base: &[u8], edited: &[u8], limits: InspectLimits) -> Result<Applied, ApplyError> {
    match catch_unwind(AssertUnwindSafe(|| run(base, edited, limits))) {
        Ok(result) => result,
        Err(_) => Err(ApplyError::Refused("internal error while applying the marks".into())),
    }
}

/// The deadline's time left.
struct Clock {
    #[cfg(not(target_arch = "wasm32"))]
    end: Option<std::time::Instant>,
}

impl Clock {
    fn new(limits: InspectLimits) -> Self {
        let _ = limits;
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            end: std::time::Instant::now().checked_add(limits.deadline),
        }
    }

    fn left(&self, limits: InspectLimits) -> Result<InspectLimits, ApplyError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(end) = self.end else { return Ok(limits) };
            let left = end.saturating_duration_since(std::time::Instant::now());
            if left == Duration::ZERO {
                return Err(ApplyError::TooComplex);
            }
            Ok(InspectLimits { deadline: left })
        }
        #[cfg(target_arch = "wasm32")]
        Ok(limits)
    }
}

fn run(base: &[u8], edited: &[u8], limits: InspectLimits) -> Result<Applied, ApplyError> {
    let clock = Clock::new(limits);
    if !edited.starts_with(base) {
        return Err(ApplyError::Inspect(InspectError::Edited("the edited file is not an appended update of the base".into())));
    }
    let before = inspect(base, edited, clock.left(limits)?).map_err(|error| match error {
        InspectError::TooComplex => ApplyError::TooComplex,
        other => ApplyError::Inspect(other),
    })?;
    if before.marked_areas.is_empty() {
        return Err(ApplyError::NothingToApply);
    }
    let mut doc = Document::open(Arc::new(edited.to_vec())).map_err(|error| ApplyError::Inspect(InspectError::Edited(error.to_string())))?;
    let marks = pdfcraft_redact::marks(&doc);
    let areas: Vec<(u32, [f32; 4])> =
        marks.iter().flat_map(|mark| mark.rects.iter().map(|rect| (count(mark.page.saturating_add(1)), rect.map(|v| v as f32)))).collect();
    let newest = doc.object_numbers().last().copied().unwrap_or(0);
    let report =
        pdfcraft_redact::apply_with(&mut doc, None, pdfcraft_redact::Options { black: true, document_copies: true }).map_err(
            |error| match error {
                pdfcraft_redact::RedactError::NothingToApply => ApplyError::NothingToApply,
                pdfcraft_redact::RedactError::TooComplex => ApplyError::TooComplex,
                other => ApplyError::Refused(other.to_string()),
            },
        )?;
    clock.left(limits)?;
    normalise(&mut doc, newest)?;
    let bytes = pdfcraft_cos::write_full(&doc, &SaveOptions { mod_date: None, ..SaveOptions::default() })
        .map_err(|error| ApplyError::Refused(error.to_string()))?;
    drop(doc);
    let inspection = inspect(edited, &bytes, clock.left(limits)?).map_err(|error| match error {
        InspectError::TooComplex => ApplyError::TooComplex,
        other => ApplyError::SelfCheck(format!("{other:?}")),
    })?;
    let facts = AppliedFacts {
        marks: count(report.marks),
        pages: count(report.pages),
        areas,
        glyphs: count(report.glyphs),
        images_removed: count(report.images_removed),
        images_cleared: count(report.images_cleared),
        paths_removed: count(report.paths_removed),
        paths_clipped: count(report.paths_clipped),
        forms_rewritten: count(report.forms_rewritten),
        forms_removed: count(report.forms_removed),
        annotations_removed: count(report.annotations),
        form_fields_removed: count(report.fields),
        tags: count(report.tags),
        thumbnails: count(report.thumbnails),
        resources_removed: count(report.resources_removed),
        bookmarks_cleared: count(report.bookmarks_cleared),
        information_cleared: count(report.information_cleared),
        metadata_removed: count(report.metadata_removed),
        destinations_renamed: count(report.destinations_renamed),
    };
    self_check(&inspection, &facts, &before)?;
    canonical_streams(edited, &bytes)?;
    Ok(Applied { bytes, inspection, facts })
}

/// The output as the writer must write it: a fresh file identifier (the writer makes one of
/// its own length), and every content stream the redaction wrote (objects numbered past
/// `newest`) re-emitted from its operators as the content serializer writes them.
fn normalise(doc: &mut Document, newest: u32) -> Result<(), ApplyError> {
    doc.trailer_mut().remove(b"ID");
    let mut budget = Budget::new(Limits::default());
    for num in doc.object_numbers().into_iter().filter(|num| *num > newest) {
        let Ok(object) = doc.try_get(num) else { continue };
        let Object::Stream(stream) = &*object else { continue };
        let content = stream.dict.name(b"Subtype") == Some(b"Form") || stream.dict.int(b"PatternType") == Some(1) || is_page_content(doc, num);
        if !content {
            continue;
        }
        let Ok(data) = stream.decoded() else { continue };
        let canonical = operators::canonical(&data, &mut budget)
            .map_err(|_| ApplyError::Refused("the redaction wrote content that cannot be read again".into()))?;
        if canonical != data {
            let mut dict: Dict = stream.dict.clone();
            for key in [&b"Length"[..], b"Filter", b"DecodeParms", b"DL"] {
                dict.remove(key);
            }
            doc.set(ObjRef::new(num, 0), Object::Stream(Stream::flate(dict, &canonical)));
        }
    }
    Ok(())
}

/// Whether object `num` is one of a page's content streams.
fn is_page_content(doc: &Document, num: u32) -> bool {
    pdfcraft_model::pages(doc).iter().any(|page| match page.dict.get(b"Contents").map(|c| (c.clone(), doc.resolve(c))) {
        Some((Object::Ref(r), _)) if r.num == num => true,
        Some((_, resolved)) => resolved.as_array().is_some_and(|items| items.iter().any(|item| item.as_ref().is_some_and(|r| r.num == num))),
        None => false,
    })
}

/// The output's inspection against the edited document: the marks applied, nothing else.
fn self_check(inspection: &Inspection, facts: &AppliedFacts, before: &Inspection) -> Result<(), ApplyError> {
    let refuse = |what: &str| Err(ApplyError::SelfCheck(what.into()));
    let pages = &inspection.pages;
    if inspection.base_kept {
        return refuse("the output keeps the edited file's bytes");
    }
    if pages.before != pages.after || pages.rotated != 0 || pages.inserted != 0 || pages.deleted != 0 || pages.moved != 0 {
        return refuse("pages changed");
    }
    if inspection.content.pages_changed_outside_redactions != 0
        || inspection.content.pages_with_images_added != 0
        || inspection.text.pages_with_text_added != 0
    {
        return refuse("a page changed outside the marked areas");
    }
    let annotations = &inspection.annotations;
    if annotations.stamps != 0 || annotations.drawings != 0 || annotations.comments != 0 || annotations.redaction_marks != 0 || annotations.other != 0
    {
        return refuse("an annotation was added");
    }
    if annotations.removed != 0 || annotations.changed != 0 {
        return refuse("an annotation not under a mark was removed or changed");
    }
    if annotations.redactions_applied != facts.marks {
        return refuse("a mark was not applied over all its areas");
    }
    if inspection.form_fields_changed != 0 {
        return refuse("a form field changed");
    }
    let outlines = &inspection.outlines;
    if outlines.added != 0 || outlines.removed != 0 || outlines.destinations_changed != 0 || outlines.titles_changed != facts.bookmarks_cleared {
        return refuse("the outline changed otherwise than its titles cleared");
    }
    if inspection.destinations.named != 0 || inspection.destinations.links != 0 || inspection.destinations.renamed != facts.destinations_renamed {
        return refuse("a destination changed otherwise than renamed");
    }
    if inspection.labels.followed != 0 {
        return refuse("page labels changed");
    }
    if count(inspection.info.changed.len()) != facts.information_cleared {
        return refuse("the document information changed otherwise than entries cleared");
    }
    if !inspection.marked_areas.is_empty() {
        return refuse("a mark is left");
    }
    // P6-35: every mark quadrilateral applied, within a point, and nothing filled beyond them.
    const SLACK: f32 = 1.0;
    let near = |a: &[f32; 4], b: &[f32; 4]| a.iter().zip(b).all(|(x, y)| (x - y).abs() <= SLACK);
    let mut left = inspection.applied_areas.clone();
    for (page, area) in &facts.areas {
        let Some(at) = left.iter().position(|(on, applied)| on == page && near(applied, area)) else {
            return refuse("a mark area was not applied whole");
        };
        left.swap_remove(at);
    }
    if !left.is_empty() {
        return refuse("an area was applied that no mark asked for");
    }
    // Every box found filled lies within the marks (a render pixel of slack besides).
    for (page, area) in &inspection.redaction_areas {
        let marked: Vec<&[f32; 4]> = facts.areas.iter().filter(|(on, _)| on == page).map(|(_, area)| area).collect();
        let covered = |x: f32, y: f32| marked.iter().any(|m| x >= m[0] - 1.5 && x <= m[2] + 1.5 && y >= m[1] - 1.5 && y <= m[3] + 1.5);
        let steps = 40;
        for i in 0..=steps {
            for j in 0..=steps {
                let x = area[0] + (area[2] - area[0]) * i as f32 / steps as f32;
                let y = area[1] + (area[3] - area[1]) * j as f32 / steps as f32;
                if !covered(x, y) {
                    return refuse("an area was filled beyond the marks");
                }
            }
        }
    }
    // The marks of the update are what its own inspection reported.
    if count(before.marked_areas.len()) != count(facts.areas.len()) {
        return refuse("the marks read otherwise than the update's inspection");
    }
    Ok(())
}

/// Every content stream of the output (pages', forms', tiling patterns') that is no content
/// stream of the edited file is exactly as the content serializer writes it, byte for byte.
fn canonical_streams(edited: &[u8], output: &[u8]) -> Result<(), ApplyError> {
    let refuse = |what: &str| Err(ApplyError::SelfCheck(what.into()));
    let open = |bytes: &[u8]| Document::open(Arc::new(bytes.to_vec())).map_err(|error| ApplyError::SelfCheck(error.to_string()));
    let (edited, output) = (open(edited)?, open(output)?);
    let mut budget = Budget::new(Limits::default());
    let digests = |doc: &Document, budget: &mut Budget| -> Result<std::collections::HashSet<[u8; 32]>, ApplyError> {
        let mut out = std::collections::HashSet::new();
        for num in doc.object_numbers() {
            budget.tick().map_err(|_| ApplyError::TooComplex)?;
            if let Ok(object) = doc.try_get(num)
                && let Object::Stream(stream) = &*object
                && let Ok(data) = stream.decoded()
            {
                out.insert(sha256(&[&data]));
            }
        }
        Ok(out)
    };
    let known = digests(&edited, &mut budget)?;
    for num in output.object_numbers() {
        budget.tick().map_err(|_| ApplyError::TooComplex)?;
        let Ok(object) = output.try_get(num) else { continue };
        let Object::Stream(stream) = &*object else { continue };
        let content = stream.dict.name(b"Subtype") == Some(b"Form") || stream.dict.int(b"PatternType") == Some(1) || is_page_content(&output, num);
        if !content {
            continue;
        }
        let Ok(data) = stream.decoded() else { return refuse("a content stream cannot be decoded") };
        if known.contains(&sha256(&[&data])) {
            continue;
        }
        match operators::canonical(&data, &mut budget) {
            Ok(canonical) if canonical == data => {}
            _ => return refuse("a content stream is not as the content serializer writes it"),
        }
    }
    Ok(())
}
