//! Embed build (R6b-49, P6-32, P6-33, P6-35): a save's redaction marks applied on the host's
//! server, never in the browser. The embed build marks areas (`Redact` annotations in an
//! appended update) and never applies them; the host inspects the update, then calls
//! [`apply_marks`], which applies the marks to the edited document with the same redactor,
//! writes the result whole and canonically, and checks its own output with the inspector against
//! the edited document: the output is accepted only as exactly the marks applied, nothing else
//! changed. Any rewrite a browser uploads is refused by the host.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

use pdfcraft_cos::{Dict, Document, ObjRef, Object, SaveOptions, Stream};

use super::structure::sha256;
use super::{Budget, Fail, FieldsRemovedByRedaction, InspectError, InspectLimits, Inspection, Limits, RemovedByRedaction, inspect, operators};

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
    /// Follow-up 9 (R6b-54, N4, N8): the form's XFA removed (0 or 1); annotations elsewhere
    /// whose `/Contents`, `/RC` or `/T` held a removed word, cleared; optional-content groups
    /// whose name held one, renamed "[Redacted]"; page-label prefixes holding one, removed.
    pub xfa_removed: u32,
    pub annotation_texts_cleared: u32,
    pub layer_names_cleared: u32,
    pub label_prefixes_cleared: u32,
    /// Follow-up 9 (R6b-54, N6): the most an image's cleared pixels reach past the marks, in
    /// points: the larger side of the largest image pixel cleared (0 without one). Every pixel a
    /// mark reaches is cleared whole; the self-check allows bands filled this far past the marks.
    pub overhang: f32,
}

/// Why marks could not be applied. The host keeps the edited file unsaved on every one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The edited file is not an appended update of the base that passes inspection.
    Inspect(InspectError),
    /// It adds no redaction mark (a base's own unapplied marks are never applied).
    NothingToApply,
    /// The redactor refused, for a reason a host words (R6b-54): content under a mark it will
    /// not guess about.
    Refused(pdfcraft_redact::Refusal),
    /// The output failed its own inspection: something besides the marks applied changed.
    SelfCheck(String),
    /// A work bound or the deadline was exceeded.
    TooComplex,
}

impl ApplyError {
    /// The code a host words (`snake_case`, stable; a closed list: [`ApplyError::CODES`]).
    pub fn code(&self) -> &'static str {
        match self {
            ApplyError::Inspect(_) => "not_an_update",
            ApplyError::NothingToApply => "nothing_to_apply",
            ApplyError::Refused(refusal) => refusal.code(),
            ApplyError::SelfCheck(_) => "self_check_failed",
            ApplyError::TooComplex => "too_complex",
        }
    }

    /// Every code [`ApplyError::code`] gives.
    pub const CODES: [&'static str; 13] = [
        "not_an_update",
        "nothing_to_apply",
        "pattern_or_mask_under_mark",
        "shading_under_mark",
        "unreadable_marked_text",
        "path_not_clippable",
        "too_complex",
        "image_not_clearable",
        "text_not_placeable",
        "content_not_followable",
        "content_unreadable",
        "added_annotation_under_mark",
        "self_check_failed",
    ];
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Apply the redaction marks `edited` adds (to the base, as an appended update holding them).
/// Bounded as [`inspect`] is: `limits.deadline` covers the whole call natively (both
/// inspections, the redaction between them, which checks the clock as it works, and the
/// self-check); the redactor has a work bound of its own, and every inspection its work bounds.
///
/// In order:
/// - `inspect(base, edited)` must succeed with `edited` an appended update of `base` adding at
///   least one mark (each with a usable area); a mark the base already held is the base's
///   annotation and is not applied;
/// - the marks the update added are applied (`pdfcraft_redact::apply_only`, every area filled
///   black, overlays ignored, document-level copies of the removed words cleared, content that
///   cannot be removed exactly refused by its reason);
/// - the document is written whole: the file identifier made fresh, every content stream the
///   redaction wrote re-emitted from its operators as the content serializer writes them;
/// - the output is inspected against `edited`, and accepted only as a rewrite with every mark
///   applied (its areas, by value and in order, the update's marked areas) and nothing else
///   changed: no page added, removed, moved or rotated, nothing outside the redactions, no text
///   or image added, no annotation added, removed or changed but the marks and what the one
///   under-mark rule removes with them, no form field changed but those removed whole, no
///   outline, destination or page-label change but the copies cleared, no document information
///   change but entries removed; every filled area within the marks; every content stream the
///   output brings exactly as the content serializer writes it, byte for byte;
/// - what the redaction removed is split by who added it (R6b-55): the update's own stamps,
///   drawings and filled fields, which must be exactly what its inspection counted under its
///   marks, and the base's own.
pub fn apply_marks(base: &[u8], edited: &[u8], limits: InspectLimits) -> Result<Applied, ApplyError> {
    match catch_unwind(AssertUnwindSafe(|| run(base, edited, limits))) {
        Ok(result) => result,
        Err(_) => Err(ApplyError::SelfCheck("internal error while applying the marks".into())),
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

    /// Every work bound, with the time left as the deadline.
    fn budget(&self, limits: InspectLimits) -> Result<Budget, ApplyError> {
        Ok(Budget::new(Limits { deadline: self.left(limits)?.deadline, ..Limits::default() }))
    }
}

fn inspection_error(error: InspectError) -> ApplyError {
    match error {
        InspectError::TooComplex => ApplyError::TooComplex,
        other => ApplyError::Inspect(other),
    }
}

fn run(base: &[u8], edited: &[u8], limits: InspectLimits) -> Result<Applied, ApplyError> {
    let clock = Clock::new(limits);
    if !edited.starts_with(base) {
        return Err(ApplyError::Inspect(InspectError::Edited("the edited file is not an appended update of the base".into())));
    }
    let (before, detail) =
        super::inspect_detailed(base, edited, Limits { deadline: clock.left(limits)?.deadline, ..Limits::default() }).map_err(inspection_error)?;
    if detail.added_marks.is_empty() {
        return Err(ApplyError::NothingToApply);
    }
    let mut doc = Document::open(Arc::new(edited.to_vec())).map_err(|error| ApplyError::Inspect(InspectError::Edited(error.to_string())))?;
    let added: std::collections::HashSet<ObjRef> = detail.added_marks.iter().copied().collect();
    let areas: Vec<(u32, [f32; 4])> = pdfcraft_redact::marks(&doc)
        .iter()
        .filter(|mark| added.contains(&mark.obj))
        .flat_map(|mark| mark.rects.iter().map(|rect| (count(mark.page.saturating_add(1)), rect.map(|v| v as f32))))
        .collect();
    let newest = doc.object_numbers().last().copied().unwrap_or(0);
    let options = pdfcraft_redact::Options {
        black: true,
        document_copies: true,
        deadline: pdfcraft_redact::Deadline::after(clock.left(limits)?.deadline),
        exact: true,
    };
    let report = pdfcraft_redact::apply_only(&mut doc, &detail.added_marks, options).map_err(|error| match error {
        pdfcraft_redact::RedactError::NothingToApply => ApplyError::NothingToApply,
        pdfcraft_redact::RedactError::TooComplex => ApplyError::TooComplex,
        pdfcraft_redact::RedactError::Refused(refusal) => ApplyError::Refused(refusal),
        pdfcraft_redact::RedactError::Unreadable(_) => ApplyError::Refused(pdfcraft_redact::Refusal::ContentUnreadable),
        other => ApplyError::SelfCheck(other.to_string()),
    })?;
    // Follow-up 9 (R6b-55): what the redaction removed, by who added it. A save's own
    // annotation the host does not count under marks (not a stamp, not a drawing) cannot be
    // reported, so the redaction is refused by that reason.
    let mut removed = RemovedByRedaction::default();
    for id in &report.removed_annotations {
        match detail.added_annotations.get(id).map(Vec::as_slice) {
            None => removed.earlier = removed.earlier.saturating_add(1),
            Some(b"Stamp") => removed.stamps = removed.stamps.saturating_add(1),
            Some(b"Ink" | b"Line" | b"Square" | b"Circle" | b"Polygon" | b"PolyLine") => removed.drawings = removed.drawings.saturating_add(1),
            Some(_) => return Err(ApplyError::Refused(pdfcraft_redact::Refusal::AddedAnnotationUnderMark)),
        }
    }
    let mut fields = FieldsRemovedByRedaction::default();
    for name in &report.removed_fields {
        if detail.filled_fields.contains(name) {
            fields.filled = fields.filled.saturating_add(1);
        } else {
            fields.earlier = fields.earlier.saturating_add(1);
        }
    }
    clock.left(limits)?;
    let mut budget = clock.budget(limits)?;
    let contents = page_content_numbers(&doc, &mut budget)?;
    normalise(&mut doc, newest, &contents, &mut budget)?;
    let bytes = pdfcraft_cos::write_full(&doc, &SaveOptions { mod_date: None, ..SaveOptions::default() })
        .map_err(|error| ApplyError::SelfCheck(error.to_string()))?;
    drop(doc);
    let mut inspection = inspect(edited, &bytes, clock.left(limits)?).map_err(|error| match error {
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
        overhang: report.overhang as f32,
        xfa_removed: count(report.xfa_removed),
        annotation_texts_cleared: count(report.annotation_texts_cleared),
        layer_names_cleared: count(report.layer_names_cleared),
        label_prefixes_cleared: count(report.label_prefixes_cleared),
    };
    let mut budget = clock.budget(limits)?;
    self_check(&inspection, &facts, &before, &detail, (removed, fields), &mut budget)?;
    left_under_marks(&bytes, &facts.areas, facts.overhang)?;
    clock.left(limits)?;
    canonical_streams(edited, &bytes, &mut budget)?;
    inspection.annotations.removed_by_redaction = removed;
    inspection.form_fields_removed_by_redaction = fields;
    Ok(Applied { bytes, inspection, facts })
}

/// The numbers of every page's content streams (built once: R6b-55, inspector re-review N6).
fn page_content_numbers(doc: &Document, budget: &mut Budget) -> Result<std::collections::HashSet<u32>, ApplyError> {
    let mut numbers = std::collections::HashSet::new();
    for page in pdfcraft_model::pages(doc) {
        budget.tick().map_err(|_| ApplyError::TooComplex)?;
        match page.dict.get(b"Contents").map(|c| (c.clone(), doc.resolve(c))) {
            Some((Object::Ref(r), resolved)) if !matches!(&*resolved, Object::Array(_)) => {
                numbers.insert(r.num);
            }
            Some((_, resolved)) => {
                for item in resolved.as_array().map(Vec::as_slice).unwrap_or_default() {
                    budget.tick().map_err(|_| ApplyError::TooComplex)?;
                    if let Some(r) = item.as_ref() {
                        numbers.insert(r.num);
                    }
                }
            }
            None => {}
        }
    }
    Ok(numbers)
}

/// The output as the writer must write it: a fresh file identifier (the writer makes one of
/// its own length), and every content stream the redaction wrote (objects numbered past
/// `newest`) re-emitted from its operators as the content serializer writes them.
fn normalise(doc: &mut Document, newest: u32, contents: &std::collections::HashSet<u32>, budget: &mut Budget) -> Result<(), ApplyError> {
    doc.trailer_mut().remove(b"ID");
    for num in doc.object_numbers().into_iter().filter(|num| *num > newest) {
        budget.tick().map_err(|_| ApplyError::TooComplex)?;
        let Ok(object) = doc.try_get(num) else { continue };
        let Object::Stream(stream) = &*object else { continue };
        let content = stream.dict.name(b"Subtype") == Some(b"Form") || stream.dict.int(b"PatternType") == Some(1) || contents.contains(&num);
        if !content {
            continue;
        }
        let Ok(data) = stream.decoded() else { continue };
        let canonical = operators::canonical(&data, budget).map_err(|fail| match fail {
            Fail::TooComplex => ApplyError::TooComplex,
            _ => ApplyError::SelfCheck("the redaction wrote content that cannot be read again".into()),
        })?;
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

/// Follow-up 9 (R6b-54, N1): the output read again, page by page: nothing the redactor reads as
/// drawn under an applied area (a glyph, an inline image, a point or control point of a path)
/// but its own boxes.
pub(crate) fn left_under_marks(output: &[u8], areas: &[(u32, [f32; 4])], overhang: f32) -> Result<(), ApplyError> {
    let mut doc = Document::open(Arc::new(output.to_vec())).map_err(|error| ApplyError::SelfCheck(error.to_string()))?;
    let mut by_page: std::collections::BTreeMap<u32, Vec<[f64; 4]>> = std::collections::BTreeMap::new();
    for (page, area) in areas {
        by_page.entry(*page).or_default().push(area.map(f64::from));
    }
    // Audit (follow-up 9): the stream the reading below leaves out as the redactor's boxes
    // (`/PCMark /Redaction`) holds nothing but its boxes: black, filled, each within the marks of
    // its page (or the image overhang past them), on every page.
    let reach = f64::from(BAND_SLACK + overhang);
    let mut budget = Budget::new(Limits::default());
    for (index, page) in pdfcraft_model::pages(&doc).iter().enumerate() {
        let number = count(index.saturating_add(1));
        let marks = by_page.get(&number).map(Vec::as_slice).unwrap_or_default();
        let list = match page.dict.get(b"Contents").map(|c| (*doc.resolve(c)).clone()) {
            Some(Object::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        for item in list {
            let Object::Stream(stream) = &*doc.resolve(&item) else { continue };
            if stream.dict.name(b"PCMark") != Some(b"Redaction") {
                continue;
            }
            let data = stream.decoded().map_err(|error| ApplyError::SelfCheck(error.to_string()))?;
            let canonical = operators::canonical(&data, &mut budget).map_err(|_| ApplyError::SelfCheck("the boxes cannot be read".into()))?;
            for line in String::from_utf8_lossy(&canonical).lines() {
                let words: Vec<&str> = line.split_whitespace().collect();
                let boxed = match words.as_slice() {
                    ["q"] | ["Q"] | ["f"] | ["0", "0", "0", "rg"] | ["0", "g"] => None,
                    [x, y, w, h, "re"] => {
                        let numbers: Option<Vec<f64>> = [x, y, w, h].iter().map(|v| v.parse::<f64>().ok()).collect();
                        match numbers.as_deref() {
                            Some(&[x, y, w, h]) => Some([x.min(x + w), y.min(y + h), x.max(x + w), y.max(y + h)]),
                            _ => return Err(ApplyError::SelfCheck("the boxes hold something else".into())),
                        }
                    }
                    _ => return Err(ApplyError::SelfCheck(format!("the boxes on page {number} hold something else than boxes"))),
                };
                if let Some(b) = boxed
                    && !marks.iter().any(|m| b[0] >= m[0] - reach && b[1] >= m[1] - reach && b[2] <= m[2] + reach && b[3] <= m[3] + reach)
                {
                    return Err(ApplyError::SelfCheck(format!("a box on page {number} lies outside the marks")));
                }
            }
        }
    }
    for (page, rects) in by_page {
        let index = usize::try_from(page.saturating_sub(1)).unwrap_or(usize::MAX);
        match pdfcraft_redact::residue(&mut doc, index, &rects) {
            Ok(0) => {}
            Ok(_) => return Err(ApplyError::SelfCheck(format!("something is left under a mark on page {page}"))),
            Err(pdfcraft_redact::RedactError::TooComplex) => return Err(ApplyError::TooComplex),
            Err(error) => return Err(ApplyError::SelfCheck(error.to_string())),
        }
    }
    Ok(())
}

/// The slack, in points, within which an applied area matches its mark area, and a filled
/// band lies within the marks (a render pixel at the comparison's resolution, and its edge).
const AREA_SLACK: f32 = 1.0;
const BAND_SLACK: f32 = 1.5;

/// The output's inspection against the edited document: the marks applied, nothing else.
fn self_check(
    inspection: &Inspection,
    facts: &AppliedFacts,
    before: &Inspection,
    detail: &super::Detail,
    (removed, fields): (RemovedByRedaction, FieldsRemovedByRedaction),
    budget: &mut Budget,
) -> Result<(), ApplyError> {
    let refuse = |what: &str| Err(ApplyError::SelfCheck(what.into()));
    let tick = |budget: &mut Budget| budget.tick().map_err(|_| ApplyError::TooComplex);
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
    // R6b-55: the redactor and the inspector removed by the one rule, and the save's own share
    // is exactly what its inspection counted under its marks.
    if annotations.removed_by_redaction.total() != removed.total() {
        return refuse("the annotations removed with the marks are not those the redactor removed");
    }
    if inspection.form_fields_removed_by_redaction.total() != fields.total() {
        return refuse("the form fields removed with the marks are not those the redactor removed");
    }
    let under = &before.added_under_marks;
    if (removed.stamps, removed.drawings, fields.filled) != (under.stamps, under.drawings, under.filled_fields) {
        return refuse("what the save added under its marks is not what the redaction removed of it");
    }
    if facts.marks != count(detail.added_marks.len()) || annotations.redactions_applied != facts.marks {
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
    // Follow-up 9 (R6b-54, N4, N8): the copies cleared are those the redactor cleared.
    let copies = &inspection.copies_cleared;
    if (copies.xfa, copies.annotation_texts, copies.layer_names) != (facts.xfa_removed, facts.annotation_texts_cleared, facts.layer_names_cleared)
        || (copies.label_prefixes > 0) != (facts.label_prefixes_cleared > 0)
    {
        return refuse("the copies cleared are not those the redactor cleared");
    }
    // R6b-55 (inspector re-review N8): the marks applied are the update's, area by area, by
    // value and in order (P6-35).
    if facts.areas != before.marked_areas {
        return refuse("the marks read otherwise than the update's inspection");
    }
    if inspection.applied_areas.len() != facts.areas.len() {
        return refuse("a mark area was not applied, or one no mark asked for was");
    }
    for ((page, applied), (on, area)) in inspection.applied_areas.iter().zip(&facts.areas) {
        tick(budget)?;
        if page != on || !applied.iter().zip(area).all(|(x, y)| (x - y).abs() <= AREA_SLACK) {
            return refuse("a mark area was not applied whole, or not in the update's order");
        }
    }
    // Every band found filled lies within the marks: in one mark, or (marks that overlap) in
    // their union, with the marks bucketed by page and every step on the budget (R6b-55,
    // inspector re-review N6).
    let mut by_page: std::collections::BTreeMap<u32, Vec<[f32; 4]>> = std::collections::BTreeMap::new();
    for (page, area) in &facts.areas {
        by_page.entry(*page).or_default().push(*area);
    }
    for marks in by_page.values_mut() {
        marks.sort_by(|a, b| a[0].total_cmp(&b[0]));
    }
    for (page, band) in &inspection.filled_areas {
        tick(budget)?;
        let marks = by_page.get(page).map(Vec::as_slice).unwrap_or_default();
        if !within_marks(band, marks, BAND_SLACK + facts.overhang, budget)? {
            return refuse("an area was filled beyond the marks");
        }
    }
    Ok(())
}

/// Whether `band` lies within the union of `marks` (sorted by their left edge), each widened
/// by `slack`: in one of them (the usual case, found first), or covered by those it meets, slab
/// by slab.
fn within_marks(band: &[f32; 4], marks: &[[f32; 4]], slack: f32, budget: &mut Budget) -> Result<bool, ApplyError> {
    let tick = |budget: &mut Budget| budget.tick().map_err(|_| ApplyError::TooComplex);
    let widened = |m: &[f32; 4]| [m[0] - slack, m[1] - slack, m[2] + slack, m[3] + slack];
    // The marks that can meet the band: their left edges up to its right edge.
    let reach = marks.partition_point(|m| m[0] - slack <= band[2]);
    let mut meeting: Vec<[f32; 4]> = Vec::new();
    for mark in marks.iter().take(reach) {
        tick(budget)?;
        let m = widened(mark);
        if m[0] <= band[0] && m[1] <= band[1] && m[2] >= band[2] && m[3] >= band[3] {
            return Ok(true);
        }
        if m[2] > band[0] && m[3] > band[1] && m[1] < band[3] {
            meeting.push(m);
        }
    }
    // Covered by their union: between each pair of neighbouring x edges, the marks spanning
    // that slab must cover the band's height.
    let mut edges: Vec<f32> = vec![band[0], band[2]];
    edges.extend(meeting.iter().flat_map(|m| [m[0], m[2]]).filter(|x| *x > band[0] && *x < band[2]));
    edges.sort_by(f32::total_cmp);
    edges.dedup();
    for slab in edges.windows(2) {
        let (left, right) = (slab[0], slab[1]);
        let mut spans: Vec<(f32, f32)> = Vec::new();
        for m in &meeting {
            tick(budget)?;
            if m[0] <= left && m[2] >= right {
                spans.push((m[1], m[3]));
            }
        }
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut reached = band[1];
        for (low, high) in spans {
            tick(budget)?;
            if low > reached {
                break;
            }
            reached = reached.max(high);
        }
        if reached < band[3] {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every content stream of the output (pages', forms', tiling patterns') that is no content
/// stream of the edited file is exactly as the content serializer writes it, byte for byte.
fn canonical_streams(edited: &[u8], output: &[u8], budget: &mut Budget) -> Result<(), ApplyError> {
    let refuse = |what: &str| Err(ApplyError::SelfCheck(what.into()));
    let open = |bytes: &[u8]| Document::open(Arc::new(bytes.to_vec())).map_err(|error| ApplyError::SelfCheck(error.to_string()));
    let (edited, output) = (open(edited)?, open(output)?);
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
    let known = digests(&edited, budget)?;
    let contents = page_content_numbers(&output, budget)?;
    for num in output.object_numbers() {
        budget.tick().map_err(|_| ApplyError::TooComplex)?;
        let Ok(object) = output.try_get(num) else { continue };
        let Object::Stream(stream) = &*object else { continue };
        let content = stream.dict.name(b"Subtype") == Some(b"Form") || stream.dict.int(b"PatternType") == Some(1) || contents.contains(&num);
        if !content {
            continue;
        }
        let Ok(data) = stream.decoded() else { return refuse("a content stream cannot be decoded") };
        if known.contains(&sha256(&[&data])) {
            continue;
        }
        match operators::canonical(&data, budget) {
            Ok(canonical) if canonical == data => {}
            Err(Fail::TooComplex) => return Err(ApplyError::TooComplex),
            _ => return refuse("a content stream is not as the content serializer writes it"),
        }
    }
    Ok(())
}
