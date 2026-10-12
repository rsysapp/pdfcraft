//! Follow-up 9 (R6b-52 to R6b-55): redaction through `apply_marks`, end to end. Each case is
//! one of the re-reviews' crafted inputs (the redaction half's `p1fu8-b` probes, the inspector
//! half's `p1fu8-a` updates): marked with the embed's own session (or written as an appended
//! update by hand), applied on the server, and checked for what is left by three readers (the
//! raw bytes, every object as `pdfcraft_cos` parses it, and hayro's extracted text).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use super::*;
use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};

pub(super) fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// A file of `bodies` (objects numbered from 1), the catalog first, with `trailer` entries.
pub(super) fn pdf(bodies: &[Vec<u8>], trailer: &str) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R {trailer} >>\nstartxref\n{xref}\n%%EOF\n", bodies.len() + 1).as_bytes());
    out
}

/// An appended update of `base`: `objects` (number, body) written after it, with its own
/// cross-reference section chained to the base's.
pub(super) fn append(base: &[u8], objects: &[(u32, Vec<u8>)], trailer: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(base).into_owned();
    let prev: usize = text.rsplit("startxref\n").next().and_then(|rest| rest.lines().next()).and_then(|n| n.trim().parse().ok()).unwrap();
    let size: u32 = text.rsplit("/Size ").next().and_then(|rest| rest.split_whitespace().next()).and_then(|n| n.parse().ok()).unwrap();
    let mut out = base.to_vec();
    let mut rows: Vec<(u32, usize)> = Vec::new();
    for (num, body) in objects {
        rows.push((*num, out.len()));
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    rows.sort();
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (num, offset) in &rows {
        out.extend_from_slice(format!("{num} 1\n{offset:010} 00000 n \n").as_bytes());
    }
    let size = rows.iter().map(|(num, _)| num + 1).fold(size, u32::max);
    out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R {trailer} /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

/// Helvetica with its widths, as the probes write it.
pub(super) const HELV_WIDTHS: &str = "278 278 355 556 556 889 667 191 333 333 389 584 278 333 278 278 556 556 556 556 556 556 556 556 556 556 \
    278 278 584 584 584 556 1015 667 667 722 722 667 611 778 722 278 500 667 556 833 722 778 667 778 722 667 611 722 667 944 667 667 611 \
    278 278 278 469 556 333 556 556 500 556 556 278 556 556 222 222 500 222 833 556 556 556 556 333 500 278 556 500 722 500 500 500 334 260 334 584";
pub(super) const SECRET: &str = "SECRETWORD";
pub(super) const BOX: [f64; 4] = [16.0, 144.0, 112.0, 166.0];
pub(super) const LINE: &str = "BT /F1 12 Tf 20 150 Td (SECRETWORD) Tj ET";
pub(super) const KEEP: &str = "BT /F1 12 Tf 20 250 Td (Keep) Tj ET";

/// A one-page file (200 × 300). Objects: 1 catalog, 2 pages, 3 page, 4 content, 5 Helvetica,
/// then `extras` from 6.
#[derive(Clone, Default)]
pub(super) struct Spec {
    pub content: String,
    pub res: String,
    pub fonts: String,
    pub page: String,
    pub cat: String,
    pub trailer: String,
    pub extras: Vec<Vec<u8>>,
}

impl Spec {
    pub fn new(content: &str) -> Self {
        Spec { content: content.into(), ..Spec::default() }
    }

    pub fn build(&self) -> Vec<u8> {
        let mut bodies = vec![
            format!("<< /Type /Catalog /Pages 2 0 R {} >>", self.cat).into_bytes(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec(),
            format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 5 0 R {} >> {} >> {} >>", self.fonts, self.res, self.page)
                .into_bytes(),
            stream("", self.content.as_bytes()),
            format!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding /FirstChar 32 /LastChar 126 /Widths [{HELV_WIDTHS}] >>")
                .into_bytes(),
        ];
        bodies.extend(self.extras.iter().cloned());
        pdf(&bodies, &self.trailer)
    }
}

/// The embed's session: keeps the document information, marks only, saves an appended update.
pub(super) fn session_save(base: &[u8], edits: Vec<Edit>) -> Vec<u8> {
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    session.set_redaction_marks_only(true);
    let id = session.open("base.pdf", None, Arc::new(base.to_vec()), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

pub(super) fn annotation(page: usize, shape: Shape) -> Edit {
    Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: String::new(), author: "Sari".into() })
}

pub(super) fn mark(page: usize, quads: &[[f64; 4]]) -> Edit {
    annotation(page, Shape::Redact { quads: quads.iter().map(|q| rect_quad(*q)).collect(), overlay: String::new(), look: Default::default() })
}

/// `base` with marks over `quads` on its first page, saved by the embed's session.
pub(super) fn marked(base: &[u8], quads: &[[f64; 4]]) -> Vec<u8> {
    session_save(base, vec![mark(0, quads)])
}

fn contains_folded(hay: &[u8], needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase().into_bytes();
    !needle.is_empty() && hay.windows(needle.len()).any(|window| window.to_ascii_lowercase() == needle)
}

fn all_text(object: &pdfcraft_cos::Object, out: &mut Vec<u8>) {
    use pdfcraft_cos::Object;
    match object {
        Object::String(s) => {
            out.extend_from_slice(&s.bytes);
            out.push(b' ');
            out.extend_from_slice(s.to_text().as_bytes());
        }
        Object::Name(n) => out.extend_from_slice(n),
        Object::Array(items) => items.iter().for_each(|item| all_text(item, out)),
        Object::Dict(d) => d.iter().for_each(|(key, value)| {
            out.extend_from_slice(key);
            out.push(b' ');
            all_text(value, out);
        }),
        Object::Stream(s) => {
            s.dict.iter().for_each(|(key, value)| {
                out.extend_from_slice(key);
                out.push(b' ');
                all_text(value, out);
            });
            out.push(b' ');
            out.extend_from_slice(&s.decoded().unwrap_or_default());
        }
        _ => {}
    }
    out.push(b' ');
}

/// Where `needle` can still be read (case-insensitively): the raw bytes, any object parsed and
/// decoded, the trailer, hayro's text of any page. Empty: nowhere.
pub(super) fn found(bytes: &[u8], needle: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    if contains_folded(bytes, needle) {
        found.push("raw".into());
    }
    let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
    for num in doc.object_numbers() {
        if let Ok(object) = doc.try_get(num) {
            let mut text = Vec::new();
            all_text(&object, &mut text);
            if contains_folded(&text, needle) {
                found.push(format!("object {num}"));
            }
        }
    }
    let mut text = Vec::new();
    all_text(&pdfcraft_cos::Object::Dict(doc.trailer().clone()), &mut text);
    if contains_folded(&text, needle) {
        found.push("trailer".into());
    }
    let hayro = pdfcraft_render::content::ContentDocument::open(Arc::new(bytes.to_vec()), u64::MAX).unwrap();
    for page in 0..hayro.page_count() {
        if let Ok(content) = hayro.content(page) {
            let text: String = content.glyphs.chars.iter().collect();
            if text.to_lowercase().contains(&needle.to_lowercase()) {
                found.push(format!("hayro text on page {}", page + 1));
            }
        }
    }
    found
}

/// Apply the marks of `edited`, with the default limits.
pub(super) fn apply(base: &[u8], edited: &[u8]) -> Result<Applied, ApplyError> {
    apply_marks(base, edited, InspectLimits::default())
}

// ── The update the inspector half crafted by hand (`p1fu8-a`, `gen_up.py`) ─────────────────────

const ALPHA: &str = "BT /F1 12 Tf 20 150 Td (Alpha) Tj ET\nBT /F1 12 Tf 150 20 Td (X) Tj ET";
const STAMP_AP: &str = "/Type /XObject /Subtype /Form /BBox [0 0 40 20] /Resources << >>";

/// Two pages; page 1 holds a square annotation (8) at [10 10 60 60]; the document information is 9.
fn two_pages() -> Vec<u8> {
    let bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 200 300] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 7 0 R >> >> /Annots [8 0 R] >>".to_vec(),
        stream("", ALPHA.as_bytes()),
        b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Resources << /Font << /F1 7 0 R >> >> >>".to_vec(),
        stream("", b"BT /F1 12 Tf 20 150 Td (Bravo) Tj ET"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        b"<< /Type /Annot /Subtype /Square /Rect [10 10 60 60] /C [1 0 0] /P 3 0 R >>".to_vec(),
        b"<< /Title (Agreement) /Producer (gen) >>".to_vec(),
    ];
    pdf(&bodies, "/Info 9 0 R")
}

fn page_one(annots: &str) -> (u32, Vec<u8>) {
    (3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 7 0 R >> >> /Annots {annots} >>").into_bytes())
}

/// A mark (object 10) at [140 10 170 40] over page 1's "X", with `extra` entries.
fn hand_mark(extra: &str) -> Vec<(u32, Vec<u8>)> {
    vec![
        page_one("[8 0 R 10 0 R]"),
        (
            10,
            format!(
                "<< /Type /Annot /Subtype /Redact /Rect [140 10 170 40] /QuadPoints [140 40 170 40 140 10 170 10] /IC [0 0 0] /P 3 0 R {extra} >>"
            )
            .into_bytes(),
        ),
    ]
}

fn hand_update(objects: &[(u32, Vec<u8>)]) -> (Vec<u8>, Vec<u8>) {
    let base = two_pages();
    let edited = append(&base, objects, "/Info 9 0 R");
    (base, edited)
}

#[test]
fn a_mark_counts_as_carrying_an_overlay_by_the_host_s_rule() {
    // R6b-55 (inspector re-review N7): an overlay is any /OverlayText, /RO, /Repeat, /DA or /Q,
    // an /IC that is not black (absent, [0], [0 0 0] and [0 0 0 1] are), or an /AP that is not
    // one plain /N stream drawing paths in colours.
    let plain_ap = stream(STAMP_AP, b"0 g 0 0 30 30 re f");
    let text_ap =
        stream("/Type /XObject /Subtype /Form /BBox [0 0 30 30] /Resources << /Font << /F1 7 0 R >> >>", b"BT /F1 9 Tf 0 10 Td (REPLACEMENT) Tj ET");
    type Case<'a> = (&'a str, Vec<(u32, Vec<u8>)>, u32);
    let cases: Vec<Case<'_>> = vec![
        ("plain", hand_mark(""), 0),
        ("/DA", hand_mark("/DA (/Helv 12 Tf 0 g)"), 1),
        ("/Q", hand_mark("/Q 1"), 1),
        ("/OverlayText", hand_mark("/OverlayText (Withheld)"), 1),
        ("/RO", hand_mark("/RO 11 0 R"), 1),
        ("/Repeat", hand_mark("/Repeat false"), 1),
        ("/AP drawing text", [hand_mark("/AP << /N 11 0 R >>"), vec![(11, text_ap.clone())]].concat(), 1),
        ("/AP with a /D state", [hand_mark("/AP << /N 11 0 R /D 11 0 R >>"), vec![(11, plain_ap.clone())]].concat(), 1),
        ("/AP of states", [hand_mark("/AP << /N << /On 11 0 R >> >>"), vec![(11, plain_ap.clone())]].concat(), 1),
        ("/AP plain", [hand_mark("/AP << /N 11 0 R >>"), vec![(11, plain_ap.clone())]].concat(), 0),
        (
            "/IC red",
            hand_mark("").into_iter().map(|(n, b)| (n, String::from_utf8(b).unwrap().replace("/IC [0 0 0]", "/IC [1 0 0]").into_bytes())).collect(),
            1,
        ),
        (
            "/IC absent",
            hand_mark("").into_iter().map(|(n, b)| (n, String::from_utf8(b).unwrap().replace("/IC [0 0 0]", "").into_bytes())).collect(),
            0,
        ),
        (
            "/IC CMYK black",
            hand_mark("").into_iter().map(|(n, b)| (n, String::from_utf8(b).unwrap().replace("/IC [0 0 0]", "/IC [0 0 0 1]").into_bytes())).collect(),
            0,
        ),
        (
            "/IC grey black",
            hand_mark("").into_iter().map(|(n, b)| (n, String::from_utf8(b).unwrap().replace("/IC [0 0 0]", "/IC [0]").into_bytes())).collect(),
            0,
        ),
    ];
    for (case, objects, expected) in cases {
        let (base, edited) = hand_update(&objects);
        let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!((inspection.annotations.redaction_marks, inspection.marks_with_overlay), (1, expected), "{case}");
    }
}

#[test]
fn a_mark_without_a_usable_area_is_refused() {
    // R6b-55 (inspector re-review N8): a mark with no usable area was counted but never applied.
    let objects = vec![
        page_one("[8 0 R 10 0 R 12 0 R]"),
        (10, b"<< /Type /Annot /Subtype /Redact /QuadPoints [1] /IC [0 0 0] /P 3 0 R >>".to_vec()),
        (12, b"<< /Type /Annot /Subtype /Redact /Rect [140 10 170 40] /QuadPoints [140 40 170 40 140 10 170 10] /IC [0 0 0] /P 3 0 R >>".to_vec()),
    ];
    let (base, edited) = hand_update(&objects);
    assert!(matches!(inspect(&base, &edited, InspectLimits::default()), Err(InspectError::Edited(_))));
    assert!(matches!(apply(&base, &edited), Err(ApplyError::Inspect(_))));
    // A mark whose quadrilaterals hold anything but numbers, or a zero area, has none either.
    for quads in ["[/x 140 40 170 40 140 10 170 10]", "[140 40 140 40 140 10 140 10]"] {
        let objects = vec![
            page_one("[8 0 R 10 0 R]"),
            (10, format!("<< /Type /Annot /Subtype /Redact /QuadPoints {quads} /IC [0 0 0] /P 3 0 R >>").into_bytes()),
        ];
        let (base, edited) = hand_update(&objects);
        assert!(matches!(inspect(&base, &edited, InspectLimits::default()), Err(InspectError::Edited(_))), "{quads}");
    }
}

/// The inspector half's `markbase.pdf`: a stored version holding an unapplied mark with an
/// overlay (object 8, over "Alpha").
fn base_with_a_mark() -> Vec<u8> {
    let bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 200 300] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 7 0 R >> >> /Annots [8 0 R] >>".to_vec(),
        stream("", ALPHA.as_bytes()),
        b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Resources << /Font << /F1 7 0 R >> >> >>".to_vec(),
        stream("", b"BT /F1 12 Tf 20 150 Td (Bravo) Tj ET"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        b"<< /Type /Annot /Subtype /Redact /Rect [15 145 60 165] /QuadPoints [15 165 60 165 15 145 60 145] /IC [0 0 0] /OverlayText (WITHHELD) /P 3 0 R >>"
            .to_vec(),
        b"<< /Title (Agreement) /Producer (gen) >>".to_vec(),
    ];
    pdf(&bodies, "/Info 9 0 R")
}

#[test]
fn only_the_marks_an_update_adds_count_and_are_applied() {
    // R6b-55 (inspector re-review N2): a base's unapplied mark is the base's annotation: not
    // counted, never applied unasked.
    let base = base_with_a_mark();
    let stamp = "<< /Type /Annot /Subtype /Stamp /Rect [100 100 140 120] /P 3 0 R /AP << /N 11 0 R >> >>".to_string();
    let stamp_only =
        append(&base, &[page_one("[8 0 R 10 0 R]"), (10, stamp.into_bytes()), (11, stream(STAMP_AP, b"0 0 1 rg 0 0 40 20 re f"))], "/Info 9 0 R");
    let inspection = inspect(&base, &stamp_only, InspectLimits::default()).unwrap();
    assert_eq!((inspection.annotations.redaction_marks, inspection.marks_with_overlay), (0, 0), "{inspection:?}");
    assert!(inspection.marked_areas.is_empty(), "{inspection:?}");
    assert_eq!(apply(&base, &stamp_only), Err(ApplyError::NothingToApply));

    let new_mark = append(
        &base,
        &[
            page_one("[8 0 R 10 0 R]"),
            (
                10,
                b"<< /Type /Annot /Subtype /Redact /Rect [140 10 170 40] /QuadPoints [140 40 170 40 140 10 170 10] /IC [0 0 0] /P 3 0 R >>".to_vec(),
            ),
        ],
        "/Info 9 0 R",
    );
    let inspection = inspect(&base, &new_mark, InspectLimits::default()).unwrap();
    assert_eq!((inspection.annotations.redaction_marks, inspection.marks_with_overlay), (1, 0), "{inspection:?}");
    assert_eq!(inspection.marked_areas, vec![(1, [140.0, 10.0, 170.0, 40.0])]);
    let applied = apply(&base, &new_mark).unwrap();
    assert_eq!(applied.facts.marks, 1, "{:?}", applied.facts);
    assert_eq!(applied.inspection.annotations.redactions_applied, 1, "{:?}", applied.inspection);
    // The base's mark, and "Alpha" under it, stay.
    assert!(!found(&applied.bytes, "WITHHELD").is_empty());
    assert!(!found(&applied.bytes, "Alpha").is_empty());
}

/// A mark (10) at [100 100 160 140] on blank page space and a stamp (12) at `rect`.
fn mark_and_stamp(rect: &str) -> (Vec<u8>, Vec<u8>) {
    hand_update(&[
        page_one("[8 0 R 10 0 R 12 0 R]"),
        (
            10,
            b"<< /Type /Annot /Subtype /Redact /Rect [100 100 160 140] /QuadPoints [100 140 160 140 100 100 160 100] /IC [0 0 0] /P 3 0 R >>"
                .to_vec(),
        ),
        (12, format!("<< /Type /Annot /Subtype /Stamp /Rect {rect} /P 3 0 R /AP << /N 11 0 R >> >>").into_bytes()),
        (11, stream(STAMP_AP, b"0 0 1 rg 0 0 40 20 re f")),
    ])
}

#[test]
fn one_under_mark_rule_holds_for_the_update_the_redactor_and_the_self_check() {
    // R6b-55 (inspector re-review N4, N5): an annotation is under a mark when its /Rect overlaps
    // a mark area by a positive area; what the redactor removes is judged against the applied
    // mark areas, not the bands of filled pixels, so a mark over blank page space applies too.
    for (case, rect, under) in [("inside", "[110 110 150 130]", 1), ("partly", "[150 110 190 130]", 1), ("no area", "[120 120 120 120]", 0)] {
        let (base, edited) = mark_and_stamp(rect);
        let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
        assert_eq!(inspection.added_under_marks, AddedUnderMarks { stamps: under, ..AddedUnderMarks::default() }, "{case}");
        let applied = apply(&base, &edited).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        let annotations = &applied.inspection.annotations;
        assert_eq!(annotations.removed_by_redaction, RemovedByRedaction { stamps: under, ..RemovedByRedaction::default() }, "{case}");
        assert_eq!((annotations.removed, annotations.redactions_applied), (0, 1), "{case}");
    }
    // A mark wholly, and partly, over the base's square: removed with the mark, the base's own.
    for (case, area) in [("wholly", [5.0, 5.0, 65.0, 65.0]), ("partly", [40.0, 40.0, 80.0, 80.0])] {
        let [x0, y0, x1, y1] = area;
        let (base, edited) = hand_update(&[
            page_one("[8 0 R 10 0 R]"),
            (
                10,
                format!("<< /Type /Annot /Subtype /Redact /Rect [{x0} {y0} {x1} {y1}] /QuadPoints [{x0} {y1} {x1} {y1} {x0} {y0} {x1} {y0}] /IC [0 0 0] /P 3 0 R >>")
                    .into_bytes(),
            ),
        ]);
        let applied = apply(&base, &edited).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(applied.inspection.annotations.removed_by_redaction, RemovedByRedaction { earlier: 1, ..RemovedByRedaction::default() }, "{case}");
        assert_eq!(applied.inspection.annotations.removed, 0, "{case}");
    }
}

#[test]
fn what_the_redaction_removed_is_split_by_who_added_it() {
    // R6b-55 (inspector re-review N3): the update's own stamps and drawings under its marks, and
    // the base's annotations, told apart by object identity.
    let base = Spec {
        page: "/Annots [6 0 R]".into(),
        extras: vec![b"<< /Type /Annot /Subtype /Square /Rect [20 146 60 160] /C [1 0 0] >>".to_vec()],
        ..Spec::new(&format!("{LINE}\n{KEEP}"))
    }
    .build();
    let edited = session_save(
        &base,
        vec![
            annotation(0, Shape::Stamp { rect: [70.0, 146.0, 110.0, 164.0], stamp: pdfcraft_annot::StampKind::Approved, by: None }),
            annotation(0, Shape::Ink { strokes: vec![vec![[30.0, 150.0], [60.0, 160.0]]] }),
            annotation(0, Shape::Stamp { rect: [20.0, 20.0, 60.0, 40.0], stamp: pdfcraft_annot::StampKind::Approved, by: None }),
            mark(0, &[BOX]),
        ],
    );
    let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!(inspection.added_under_marks, AddedUnderMarks { stamps: 1, drawings: 1, filled_fields: 0 }, "{inspection:?}");
    let applied = apply(&base, &edited).unwrap();
    assert_eq!(
        applied.inspection.annotations.removed_by_redaction,
        RemovedByRedaction { stamps: 1, drawings: 1, earlier: 1 },
        "{:?}",
        applied.inspection
    );
    assert_eq!(applied.inspection.annotations.removed, 0);
    assert!(found(&applied.bytes, SECRET).is_empty());
}

#[test]
fn a_comment_thread_goes_whole_with_its_comment() {
    // R6b-54 N6, P6-40 (d) (redaction re-review I4): the replies to a comment under a mark go
    // with it, wherever they are, counted as the base's own removed by the redaction.
    let base = Spec {
        page: "/Annots [6 0 R 7 0 R 8 0 R]".into(),
        extras: vec![
            b"<< /Type /Annot /Subtype /FreeText /Rect [20 148 100 162] /Contents (SECRETWORD) /DA (/Helv 12 Tf 0 g) /Popup 7 0 R >>".to_vec(),
            b"<< /Type /Annot /Subtype /Popup /Rect [120 200 190 280] /Parent 6 0 R >>".to_vec(),
            b"<< /Type /Annot /Subtype /Text /Rect [150 10 170 30] /Contents (agreed) /IRT 6 0 R >>".to_vec(),
        ],
        ..Spec::new(KEEP)
    }
    .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert_eq!(
        applied.inspection.annotations.removed_by_redaction,
        RemovedByRedaction { earlier: 2, ..RemovedByRedaction::default() },
        "{:?}",
        applied.inspection
    );
    assert_eq!(applied.inspection.annotations.removed, 0);
    assert!(found(&applied.bytes, SECRET).is_empty() && found(&applied.bytes, "agreed").is_empty());
}

#[test]
fn an_annotation_crossing_a_mark_edge_goes_with_it() {
    // R6b-54 N6, R6b-55: a link over the whole line, one word marked: removed and counted as
    // removed by the redaction (one rule on both sides: overlap).
    let base = Spec {
        page: "/Annots [6 0 R]".into(),
        extras: vec![b"<< /Type /Annot /Subtype /Link /Rect [18 146 190 164] /Border [0 0 0] /A << /S /URI /URI (http://example.com) >> >>".to_vec()],
        ..Spec::new(&format!("BT /F1 12 Tf 20 150 Td (SECRET WORDS here) Tj ET\n{KEEP}"))
    }
    .build();
    let applied = apply(&base, &marked(&base, &[[16.0, 144.0, 70.0, 166.0]])).unwrap();
    assert_eq!(
        applied.inspection.annotations.removed_by_redaction,
        RemovedByRedaction { earlier: 1, ..RemovedByRedaction::default() },
        "{:?}",
        applied.inspection
    );
    assert!(found(&applied.bytes, "SECRET ").is_empty());
}

/// A field (6) showing "FIELDVALUE" with two widgets (7 under the mark, 8 elsewhere).
fn field_with_two_widgets() -> Vec<u8> {
    Spec {
        cat: "/AcroForm << /Fields [6 0 R] /DA (/Helv 0 Tf 0 g) >>".into(),
        page: "/Annots [7 0 R 8 0 R]".into(),
        extras: vec![
            b"<< /FT /Tx /T (f1) /V (FIELDVALUE) /Kids [7 0 R 8 0 R] >>".to_vec(),
            b"<< /Type /Annot /Subtype /Widget /Parent 6 0 R /Rect [20 146 100 164] /P 3 0 R >>".to_vec(),
            b"<< /Type /Annot /Subtype /Widget /Parent 6 0 R /Rect [20 20 100 40] /P 3 0 R >>".to_vec(),
        ],
        ..Spec::new(&format!("{LINE}\n{KEEP}"))
    }
    .build()
}

#[test]
fn a_field_with_any_widget_under_a_mark_goes_whole() {
    // R6b-54 N6, R6b-55: a field with a widget under a mark and one elsewhere is removed whole,
    // with every widget, counted as the base's own field removed by the redaction.
    let base = field_with_two_widgets();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert_eq!(
        applied.inspection.form_fields_removed_by_redaction,
        FieldsRemovedByRedaction { earlier: 1, ..FieldsRemovedByRedaction::default() },
        "{:?}",
        applied.inspection
    );
    assert_eq!(applied.inspection.form_fields_changed, 0);
    assert!(found(&applied.bytes, "FIELDVALUE").is_empty());
    // Filled by the update itself, the field is the update's own.
    let edited = session_save(
        &base,
        vec![Edit::SetFieldValue { name: "f1".into(), value: pdfcraft_forms::FieldValue::Text("Filled in".into()) }, mark(0, &[BOX])],
    );
    let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!(inspection.added_under_marks, AddedUnderMarks { filled_fields: 1, ..AddedUnderMarks::default() }, "{inspection:?}");
    let applied = apply(&base, &edited).unwrap();
    assert_eq!(applied.inspection.form_fields_removed_by_redaction, FieldsRemovedByRedaction { filled: 1, earlier: 0 }, "{:?}", applied.inspection);
    assert!(found(&applied.bytes, "Filled in").is_empty());
}

/// The inspector half's `gen_many.py`: `count` marks on one page, each over one glyph.
fn many_marks(count: usize) -> (Vec<u8>, Vec<u8>) {
    let columns = 50;
    let rows = count.div_ceil(columns);
    let (width, height) = (20 * columns + 20, 20 * rows + 20);
    let mut content = b"BT /F1 10 Tf\n".to_vec();
    for i in 0..count {
        content.extend_from_slice(format!("1 0 0 1 {} {} Tm (X) Tj\n", 10 + 20 * (i % columns), 10 + 20 * (i / columns)).as_bytes());
    }
    content.extend_from_slice(b"ET");
    let base = pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!("<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 {width} {height}] >>").into_bytes(),
            b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
            stream("", &content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ],
        "",
    );
    let refs: Vec<String> = (0..count).map(|i| format!("{} 0 R", 10 + i)).collect();
    let mut objects = vec![(
        3u32,
        format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> /Annots [{}] >>", refs.join(" ")).into_bytes(),
    )];
    for i in 0..count {
        let (x0, y0) = (10 + 20 * (i % columns) - 2, 10 + 20 * (i / columns) - 3);
        let (x1, y1) = (x0 + 11, y0 + 14);
        objects.push((
            u32::try_from(10 + i).unwrap(),
            format!("<< /Type /Annot /Subtype /Redact /Rect [{x0} {y0} {x1} {y1}] /QuadPoints [{x0} {y1} {x1} {y1} {x0} {y0} {x1} {y0}] /IC [0 0 0] /P 3 0 R >>")
                .into_bytes(),
        ));
    }
    let edited = append(&base, &objects, "");
    (base, edited)
}

#[test]
fn many_marks_finish_or_stop_within_the_deadline() {
    // R6b-55 (inspector re-review N6): 5,000 marks on one page, a 2 s deadline: the self-check is
    // bucketed and on the budget, the redactor checks the clock: `TooComplex`, or done, within
    // the deadline (and a little for the last unit of work).
    let (base, edited) = many_marks(5_000);
    let deadline = std::time::Duration::from_secs(2);
    let started = std::time::Instant::now();
    let result = apply_marks(&base, &edited, InspectLimits { deadline });
    let took = started.elapsed();
    eprintln!("5,000 marks: {:?} in {took:?}", result.as_ref().map(|applied| applied.facts.marks));
    assert!(matches!(result, Ok(_) | Err(ApplyError::TooComplex)), "{:?}", result.err());
    assert!(took < deadline + std::time::Duration::from_millis(750), "{took:?}");
    // And without the deadline, it is applied: every mark, each area in the update's order.
    let (base, edited) = many_marks(300);
    let applied = apply(&base, &edited).unwrap();
    let before = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!(applied.facts.areas, before.marked_areas);
    assert_eq!(applied.inspection.applied_areas.len(), before.marked_areas.len());
}

// ── R6b-54 N6, N11: ordinary redactions the server refused, delivered end to end ─────────────

#[test]
fn content_ending_with_an_unbalanced_save_is_redacted() {
    // R6b-54 N6 (`unbalanced-tail-*`): the box is drawn after closing every save the content
    // left open, so it is neither transformed, clipped nor translucent. (A page drawn with a
    // blend mode other than /Normal stays refused, balanced or not: other viewers may blend it
    // otherwise than the renders show.)
    for (case, tail) in [("cm", "2 0 0 2 0 0 cm q"), ("clip", "0 0 10 10 re W n q"), ("alpha", "/G0 gs q"), ("deep", "q q 2 0 0 2 0 0 cm q")] {
        let base = Spec { res: "/ExtGState << /G0 << /ca 0.05 >> >>".into(), ..Spec::new(&format!("{LINE}\n{KEEP}\n{tail}")) }.build();
        let applied = apply(&base, &marked(&base, &[BOX])).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert!(found(&applied.bytes, SECRET).is_empty(), "{case}");
        assert_eq!(applied.inspection.applied_areas.len(), 1, "{case}");
    }
}

fn type3_font(widths: &str) -> Vec<u8> {
    format!(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 7 0 R /b 8 0 R >> /Encoding << /Type /Encoding /Differences [97 /a /b] >> /FirstChar 97 /LastChar 98 /Widths [{widths}] >>"
    )
    .into_bytes()
}

#[test]
fn type3_glyphs_under_a_mark_go_like_any_glyph() {
    // R6b-54 N6 (`type3-plain`, `type3-only-under`): a Type 3 glyph under a mark is removed;
    // a Type 3 font then showing nothing leaves the file with its glyph procedures.
    let procedures =
        || vec![type3_font("600 600"), stream("", b"600 0 d0 0 0 500 700 re f"), stream("", b"600 0 d0 0 0 300 300 re f 300 300 300 300 re f")];
    let both = Spec {
        fonts: "/T3 6 0 R".into(),
        extras: procedures(),
        ..Spec::new(&format!("BT /T3 12 Tf 20 150 Td (b) Tj ET\nBT /T3 12 Tf 150 250 Td (a) Tj ET\n{KEEP}"))
    }
    .build();
    let applied = apply(&both, &marked(&both, &[BOX])).unwrap();
    assert_eq!(applied.facts.glyphs, 1, "{:?}", applied.facts);
    let only = Spec { fonts: "/T3 6 0 R".into(), extras: procedures(), ..Spec::new(&format!("BT /T3 12 Tf 20 150 Td (b) Tj ET\n{KEEP}")) }.build();
    let applied = apply(&only, &marked(&only, &[BOX])).unwrap();
    assert!(found(&applied.bytes, "300 300 300 300").is_empty());
}

/// A 240 × 40 grey image (its samples spell SECRETWORD) placed at 0,145 on 120 × 20 points, with
/// `extra` entries and `second` as object 7.
fn fine_image(extra: &str, second: Option<Vec<u8>>) -> Vec<u8> {
    let pixels: Vec<u8> = SECRET.bytes().cycle().take(240 * 40).collect();
    let image = stream(&format!("/Type /XObject /Subtype /Image /Width 240 /Height 40 /ColorSpace /DeviceGray /BitsPerComponent 8 {extra}"), &pixels);
    let mut extras = vec![image];
    extras.extend(second);
    Spec {
        res: "/XObject << /Im1 6 0 R >>".into(),
        extras,
        ..Spec::new(&format!("q 120 0 0 20 0 145 cm /Im1 Do Q\nBT /F1 12 Tf 20 150 Td (Alpha) Tj ET\n{KEEP}"))
    }
    .build()
}

#[test]
fn masked_images_are_cleared_with_their_masks() {
    // R6b-54 N6 (`fine-image-*`; redaction re-review I2): an image partly under a mark with a
    // soft mask, a stencil mask, alternates or XMP is cleared, its mask with it, its copies of
    // itself and its metadata gone.
    let grey: Vec<u8> = SECRET.bytes().cycle().take(240 * 40).collect();
    let soft_mask = stream("/Type /XObject /Subtype /Image /Width 240 /Height 40 /ColorSpace /DeviceGray /BitsPerComponent 8", &grey);
    let stencil = stream("/Type /XObject /Subtype /Image /Width 240 /Height 40 /ImageMask true /BitsPerComponent 1", &vec![0x55u8; 30 * 40]);
    let xmp = stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta>SECRETWORD</x:xmpmeta>");
    for (case, extra, second) in [
        ("none", "", None),
        ("soft mask", "/SMask 7 0 R", Some(soft_mask.clone())),
        ("stencil mask", "/Mask 7 0 R", Some(stencil)),
        ("alternates", "/Alternates [<< /Image 7 0 R >>]", Some(soft_mask)),
        ("metadata", "/Metadata 7 0 R", Some(xmp)),
    ] {
        let base = fine_image(extra, second);
        let applied = apply(&base, &marked(&base, &[BOX])).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert!(applied.facts.images_cleared >= 1, "{case}: {:?}", applied.facts);
        assert!(found(&applied.bytes, "xmpmeta").is_empty() && found(&applied.bytes, "Alternates").is_empty(), "{case}");
    }
}

#[test]
fn a_coarse_image_is_cleared_by_whole_pixels_and_the_overhang_reported() {
    // R6b-54 N6 (`image-partly-none`, 6 × 10 pt pixels): every pixel a mark reaches is cleared
    // whole, so the cleared pixels reach past the mark by less than a pixel; the self-check
    // allows that much, and reports it.
    let pixels: Vec<u8> = SECRET.bytes().cycle().take(40).collect();
    let image = stream("/Type /XObject /Subtype /Image /Width 20 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8", &pixels);
    let base =
        Spec { res: "/XObject << /Im1 6 0 R >>".into(), extras: vec![image], ..Spec::new(&format!("q 120 0 0 20 0 145 cm /Im1 Do Q\n{KEEP}")) }
            .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert_eq!(applied.facts.images_cleared, 1, "{:?}", applied.facts);
    assert!(applied.facts.overhang > 0.0 && applied.facts.overhang <= 10.0, "{:?}", applied.facts);
    // Every sample a mark reaches is black.
    let doc = pdfcraft_cos::Document::open(Arc::new(applied.bytes.clone())).unwrap();
    let cleared: Vec<Vec<u8>> = doc
        .object_numbers()
        .into_iter()
        .filter_map(|num| match &*doc.try_get(num).ok()? {
            pdfcraft_cos::Object::Stream(s) if s.dict.name(b"Subtype") == Some(b"Image") => s.decoded().ok(),
            _ => None,
        })
        .collect();
    assert_eq!(cleared.len(), 1);
    // Columns 2–18 (x 6–114 pt) reach the mark (x 16–112); both rows reach it (y 145–165).
    for row in 0..2 {
        for column in 0..20 {
            let black = cleared[0][row * 20 + column] == 0;
            assert_eq!(black, (2..19).contains(&column), "row {row} column {column}");
        }
    }
}

fn image_in(colour_space: &str, extra: &str, samples: Vec<u8>, width: u32) -> Vec<u8> {
    let image =
        stream(&format!("/Type /XObject /Subtype /Image /Width {width} /Height 2 /ColorSpace {colour_space} /BitsPerComponent 8 {extra}"), &samples);
    Spec { res: "/XObject << /Im1 6 0 R >>".into(), extras: vec![image], ..Spec::new(&format!("q 120 0 0 20 0 145 cm /Im1 Do Q\n{KEEP}")) }.build()
}

#[test]
fn cleared_samples_are_the_colour_space_s_black() {
    // R6b-54 N11: cleared samples are black in the image's colour space: 0 in grey and RGB,
    // 0 0 0 1 in CMYK, the palette's darkest entry in an indexed space, and through /Decode.
    let first_cleared = |bytes: &[u8], components: usize| -> Vec<u8> {
        let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
        for num in doc.object_numbers() {
            if let Ok(object) = doc.try_get(num)
                && let pdfcraft_cos::Object::Stream(s) = &*object
                && s.dict.name(b"Subtype") == Some(b"Image")
            {
                let data = s.decoded().unwrap();
                // Column 10 (x 60 pt) lies under the mark.
                return data[10 * components..11 * components].to_vec();
            }
        }
        panic!("no image");
    };
    let cmyk = image_in("/DeviceCMYK", "", vec![40u8; 20 * 2 * 4], 20);
    let applied = apply(&cmyk, &marked(&cmyk, &[BOX])).unwrap();
    assert_eq!(first_cleared(&applied.bytes, 4), vec![0, 0, 0, 255]);
    let inverted = image_in("/DeviceGray", "/Decode [1 0]", vec![40u8; 20 * 2], 20);
    let applied = apply(&inverted, &marked(&inverted, &[BOX])).unwrap();
    assert_eq!(first_cleared(&applied.bytes, 1), vec![255]);
    let indexed = image_in("[/Indexed /DeviceRGB 2 <FFFFFF 102030 808080>]", "", vec![0u8; 20 * 2], 20);
    let applied = apply(&indexed, &marked(&indexed, &[BOX])).unwrap();
    assert_eq!(first_cleared(&applied.bytes, 1), vec![1]);
    // A space without a black the redactor can write is refused by its reason.
    let separation = image_in("[/Separation /Spot /DeviceGray << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>]", "", vec![40u8; 20 * 2], 20);
    assert_eq!(apply(&separation, &marked(&separation, &[BOX])).map(|_| ()), Err(ApplyError::Refused(Refusal::ImageNotClearable)));
}

#[test]
fn a_jpeg_partly_under_a_mark_is_refused_by_its_reason() {
    // R6b-54 N6 (`jpeg-partly`): the redactor does not re-encode JPEG data, so an image partly
    // under a mark it cannot clear is refused as such (never removed whole, which would change
    // what shows outside the mark).
    let pixels: Vec<u8> = (0..64 * 16).map(|i| (i % 251) as u8).collect();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85).encode(&pixels, 64, 16, image::ExtendedColorType::L8).unwrap();
    let image = stream("/Type /XObject /Subtype /Image /Width 64 /Height 16 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /DCTDecode", &jpeg);
    let base =
        Spec { res: "/XObject << /Im1 6 0 R >>".into(), extras: vec![image], ..Spec::new(&format!("q 120 0 0 20 0 145 cm /Im1 Do Q\n{KEEP}")) }
            .build();
    assert_eq!(apply(&base, &marked(&base, &[BOX])).map(|_| ()), Err(ApplyError::Refused(Refusal::ImageNotClearable)));
}

#[test]
fn tiling_patterns_and_soft_masks_under_a_mark_are_refused_by_their_reason() {
    // R6b-54 N6 (redaction re-review I1; `hatch-background`, `vignette-softmask`): still
    // refused, with the reason the host words.
    let hatch = Spec {
        res: "/Pattern << /P1 6 0 R >>".into(),
        extras: vec![stream("/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10", b"0.9 g 0 0 5 5 re f")],
        ..Spec::new(&format!("q /Pattern cs /P1 scn 0 0 200 300 re f Q\n{LINE}\n{KEEP}"))
    }
    .build();
    assert_eq!(apply(&hatch, &marked(&hatch, &[BOX])).map(|_| ()), Err(ApplyError::Refused(Refusal::PatternOrMaskUnderMark)));
    let vignette = Spec {
        res: "/ExtGState << /GS1 << /SMask << /S /Luminosity /G 6 0 R >> >> >>".into(),
        extras: vec![stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /CS /DeviceGray >>",
            b"0.5 g 0 0 200 300 re f",
        )],
        ..Spec::new(&format!("q /GS1 gs 0 0 1 rg 0 0 200 300 re f Q\n{LINE}\n{KEEP}"))
    }
    .build();
    assert_eq!(apply(&vignette, &marked(&vignette, &[BOX])).map(|_| ()), Err(ApplyError::Refused(Refusal::PatternOrMaskUnderMark)));
    // Every reason has its own stable code, and the list is closed.
    let codes: std::collections::BTreeSet<&str> = Refusal::ALL.iter().map(|refusal| refusal.code()).collect();
    assert_eq!(codes.len(), Refusal::ALL.len());
    assert!(codes.iter().all(|code| ApplyError::CODES.contains(code)));
}

// ── Audit (follow-up 9, coordinator's scan of images.rs): masks read as a viewer reads them ───

/// `edited` written again with `change` made to the soft mask of its first image that has one.
fn forge_soft_mask(applied: &[u8], change: impl Fn(&mut pdfcraft_cos::Dict)) -> Vec<u8> {
    let mut doc = pdfcraft_cos::Document::open(Arc::new(applied.to_vec())).unwrap();
    let mask = doc
        .object_numbers()
        .into_iter()
        .find_map(|num| match &*doc.try_get(num).ok()? {
            pdfcraft_cos::Object::Stream(s) if s.dict.name(b"Subtype") == Some(b"Image") => {
                s.dict.get(b"SMask").and_then(pdfcraft_cos::Object::as_ref)
            }
            _ => None,
        })
        .unwrap();
    let pdfcraft_cos::Object::Stream(mut stream) = (*doc.get(mask)).clone() else { panic!("no mask") };
    change(&mut stream.dict);
    doc.set(mask, pdfcraft_cos::Object::Stream(stream));
    pdfcraft_cos::write_full(&doc, &pdfcraft_cos::SaveOptions { mod_date: None, ..Default::default() }).unwrap()
}

#[test]
fn a_soft_mask_read_otherwise_than_its_samples_is_not_the_same_mask() {
    // Audit (images.rs): a cleared image's soft mask is compared sample by sample; a mask whose
    // dictionary makes a viewer read the same bytes otherwise (inverted by /Decode, another
    // depth, a /Matte) is no mask redacted in place. A white image on a white page hides the
    // change from the renders, so only the image comparison can tell.
    let ramp: Vec<u8> = (0..240 * 40).map(|i| (i % 240) as u8).collect();
    let white = stream(
        "/Type /XObject /Subtype /Image /Width 240 /Height 40 /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 7 0 R",
        &vec![255u8; 240 * 40],
    );
    let mask = stream("/Type /XObject /Subtype /Image /Width 240 /Height 40 /ColorSpace /DeviceGray /BitsPerComponent 8", &ramp);
    let base = Spec {
        res: "/XObject << /Im1 6 0 R >>".into(),
        extras: vec![white, mask],
        ..Spec::new(&format!("q 120 0 0 20 0 145 cm /Im1 Do Q\n{LINE}\n{KEEP}"))
    }
    .build();
    let edited = marked(&base, &[BOX]);
    let applied = apply(&base, &edited).unwrap();
    for (case, change) in [
        (
            "inverted",
            Box::new(|d: &mut pdfcraft_cos::Dict| {
                d.set(b"Decode".to_vec(), pdfcraft_cos::Object::Array(vec![pdfcraft_cos::Object::Int(1), pdfcraft_cos::Object::Int(0)]))
            }) as Box<dyn Fn(&mut pdfcraft_cos::Dict)>,
        ),
        ("matte", Box::new(|d: &mut pdfcraft_cos::Dict| d.set(b"Matte".to_vec(), pdfcraft_cos::Object::Array(vec![pdfcraft_cos::Object::Int(1)])))),
        ("another depth", Box::new(|d: &mut pdfcraft_cos::Dict| d.set(b"BitsPerComponent".to_vec(), pdfcraft_cos::Object::Int(4)))),
    ] {
        let forged = forge_soft_mask(&applied.bytes, change);
        if let Ok(inspection) = inspect(&edited, &forged, InspectLimits::default()) {
            assert!(inspection.content.pages_changed_outside_redactions > 0, "{case}: {inspection:?}");
        }
    }
}

#[test]
fn a_redacted_image_keeps_no_alternate_copy() {
    // Audit (images.rs): an image redacted in place may lose its alternates, XMP and private
    // data, never keep them: an alternate (shown for printing by some viewers) holds the
    // unredacted picture.
    let pixels: Vec<u8> = SECRET.bytes().cycle().take(240 * 40).collect();
    let image = stream(
        "/Type /XObject /Subtype /Image /Width 240 /Height 40 /ColorSpace /DeviceGray /BitsPerComponent 8 /Alternates [<< /Image 7 0 R >>]",
        &pixels,
    );
    let alternate = stream("/Type /XObject /Subtype /Image /Width 240 /Height 40 /ColorSpace /DeviceGray /BitsPerComponent 8", &pixels);
    let base = Spec {
        res: "/XObject << /Im1 6 0 R >>".into(),
        extras: vec![image, alternate],
        ..Spec::new(&format!("q 120 0 0 20 0 145 cm /Im1 Do Q\n{LINE}\n{KEEP}"))
    }
    .build();
    let edited = marked(&base, &[BOX]);
    let applied = apply(&base, &edited).unwrap();
    let mut doc = pdfcraft_cos::Document::open(Arc::new(applied.bytes.clone())).unwrap();
    let copy = doc.add(pdfcraft_cos::Object::Stream(pdfcraft_cos::Stream::from_raw(
        {
            let mut d = pdfcraft_cos::Dict::new();
            for (k, v) in [("Type", "XObject"), ("Subtype", "Image"), ("ColorSpace", "DeviceGray")] {
                d.set(k.as_bytes().to_vec(), pdfcraft_cos::Object::name(v));
            }
            d.set(b"Width".to_vec(), pdfcraft_cos::Object::Int(240));
            d.set(b"Height".to_vec(), pdfcraft_cos::Object::Int(40));
            d.set(b"BitsPerComponent".to_vec(), pdfcraft_cos::Object::Int(8));
            d
        },
        pixels.clone(),
    )));
    let cleared = doc
        .object_numbers()
        .into_iter()
        .find(|num| matches!(&*doc.try_get(*num).unwrap(), pdfcraft_cos::Object::Stream(s) if s.dict.name(b"Subtype") == Some(b"Image") && s.decoded().unwrap() != pixels))
        .unwrap();
    let r = pdfcraft_cos::ObjRef::new(cleared, 0);
    let pdfcraft_cos::Object::Stream(mut stream_object) = (*doc.get(r)).clone() else { panic!() };
    let mut alternate = pdfcraft_cos::Dict::new();
    alternate.set(b"Image".to_vec(), pdfcraft_cos::Object::Ref(copy));
    stream_object.dict.set(b"Alternates".to_vec(), pdfcraft_cos::Object::Array(vec![pdfcraft_cos::Object::Dict(alternate)]));
    doc.set(r, pdfcraft_cos::Object::Stream(stream_object));
    let forged = pdfcraft_cos::write_full(&doc, &pdfcraft_cos::SaveOptions { mod_date: None, ..Default::default() }).unwrap();
    if let Ok(inspection) = inspect(&edited, &forged, InspectLimits::default()) {
        assert!(inspection.content.pages_changed_outside_redactions > 0, "{inspection:?}");
    }
}

#[test]
fn an_inline_image_holding_ei_in_its_data_is_redacted_beside_or_refused() {
    // Audit (images.rs): an inline image whose data holds "EI" is read to its length by every
    // reader here; one beside a mark comes through as it was, or the redaction is refused.
    let data = b"ab EI cd";
    let mut content = format!("{LINE}\n{KEEP}\nq 40 0 0 5 120 20 cm BI /W 8 /H 1 /BPC 8 /CS /G ID ").into_bytes();
    content.extend_from_slice(data);
    content.extend_from_slice(b"\nEI Q");
    let base = Spec { content: String::from_utf8(content).unwrap(), ..Spec::default() }.build();
    if let Ok(applied) = apply(&base, &marked(&base, &[BOX])) {
        assert!(applied.bytes.windows(data.len()).any(|w| w == data) || !found(&applied.bytes, "ab EI cd").is_empty());
        assert!(found(&applied.bytes, SECRET).is_empty());
    }
}

// ── R6b-54 N1, N2, N10: paths cut exactly, shadings, vector-only redactions ───────────────────

/// Every number the output's content streams (pages' and forms') hold as an operand of a path
/// operator, as written.
fn path_operands(bytes: &[u8]) -> String {
    let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
    let mut all = String::new();
    for num in doc.object_numbers() {
        if let Ok(object) = doc.try_get(num)
            && let pdfcraft_cos::Object::Stream(s) = &*object
            && s.dict.name(b"Subtype") != Some(b"Image")
            && let Ok(data) = s.decoded()
        {
            let Ok(canonical) = operators::canonical(&data, &mut Budget::new(Limits::default())) else { continue };
            for line in String::from_utf8_lossy(&canonical).lines() {
                let mut words: Vec<&str> = line.split_whitespace().collect();
                if words.last().is_some_and(|op| matches!(*op, "m" | "l" | "c" | "v" | "y" | "re")) {
                    words.pop();
                    for word in words {
                        if let Ok(value) = word.parse::<f64>() {
                            all.push_str(&format!("{value} "));
                        }
                    }
                }
            }
        }
    }
    all
}

#[test]
fn a_path_partly_under_a_mark_loses_what_lies_under_it() {
    // R6b-54 N1 (redaction re-review N1, CRITICAL): a subpath meeting a mark goes, and what it
    // has outside is cut exactly along the mark; no point of it is left under the mark.
    // `path-two-subpaths`: one triangle wholly under the box, one outside.
    let base = Spec::new(&format!(
        "BT /F1 12 Tf 20 150 Td (Alpha) Tj ET\n0 0 1 rg 23.456 150 m 33.456 160 l 43.456 150 l h 150 150 m 160 160 l 170 150 l h f\n{KEEP}"
    ))
    .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    let operands = path_operands(&applied.bytes);
    assert!(!operands.contains("23.456") && !operands.contains("33.456") && operands.contains("150 150"), "{operands}");
    assert!(found(&applied.bytes, "23.456").is_empty());
    // `signature-path-1pt-past`: a stroke reaching 1.5 pt past the mark keeps only that.
    let base = Spec::new(&format!("0 0 0 RG 1 w 20.123 150 m 40.123 162 l 60.123 146 l 80.123 160 l 100.123 150 l 113.5 155 l S\n{KEEP}")).build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    let operands = path_operands(&applied.bytes);
    assert!(!operands.contains("40.123") && !operands.contains("60.123") && operands.contains("113.5"), "{operands}");
    // `filled-rect-crossing`: a filled shape crossing the mark: its corners outside stay, the
    // part under the mark goes, and no point is left under it (the self-check reads them).
    let base = Spec::new(&format!("{LINE}\n0 0 1 rg 30 140 m 30.5 170 l 130 170 l 130 140 l h f\n{KEEP}")).build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert!(applied.facts.paths_clipped >= 1, "{:?}", applied.facts);
}

#[test]
fn a_vector_only_redaction_counts_its_content_removed() {
    // R6b-54 N10 (`signature-path-inside`): a signature drawn as vectors, wholly under a mark,
    // is removed and counted as content removed; its area is applied.
    let base = Spec::new(&format!("0 0 0 RG 1 w 20.123 150 m 40.123 162 l 60.123 146 l 80.123 160 l 100.123 150 l S\n{KEEP}")).build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert_eq!(applied.facts.paths_removed, 1, "{:?}", applied.facts);
    assert_eq!(applied.inspection.content.pages_with_content_removed, 1, "{:?}", applied.inspection);
    assert_eq!(applied.inspection.applied_areas.len(), 1);
    assert!(!path_operands(&applied.bytes).contains("40.123"));
}

#[test]
fn a_dashed_stroke_cut_by_a_mark_is_refused_by_its_reason() {
    // R6b-54 N1: cutting a dashed stroke restarts its dashes: not exact, refused as such.
    let base = Spec::new(&format!("0 0 0 RG [3 2] 0 d 1 w 0 150 m 200 150 l S\n{KEEP}")).build();
    assert_eq!(apply(&base, &marked(&base, &[BOX])).map(|_| ()), Err(ApplyError::Refused(Refusal::PathNotClippable)));
}

fn mesh() -> Vec<u8> {
    let mut data: Vec<u8> = vec![0, 20, 145, 0, 0, 100, 145, 128, 0, 60, 165, 255];
    data.extend_from_slice(SECRET.as_bytes());
    stream("/ShadingType 4 /ColorSpace /DeviceGray /BitsPerCoordinate 8 /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 255 0 255 0 1]", &data)
}

#[test]
fn shadings_under_a_mark_go_or_are_refused_by_their_reason() {
    // R6b-54 N2 (`mesh-shading-under-box`): a shading painted only under the mark goes with its
    // data; a mesh reaching a mark otherwise is refused; a plain gradient reaching one stays.
    let under = Spec {
        res: "/Shading << /Sh1 6 0 R >>".into(),
        extras: vec![mesh()],
        ..Spec::new(&format!("BT /F1 12 Tf 20 150 Td (Alpha) Tj ET\nq 16 144 96 22 re W n /Sh1 sh Q\n{KEEP}"))
    }
    .build();
    let applied = apply(&under, &marked(&under, &[BOX])).unwrap();
    assert!(found(&applied.bytes, SECRET).is_empty(), "{:?}", found(&applied.bytes, SECRET));
    let partly = Spec {
        res: "/Shading << /Sh1 6 0 R >>".into(),
        extras: vec![mesh()],
        ..Spec::new(&format!("BT /F1 12 Tf 20 150 Td (Alpha) Tj ET\nq 0 140 200 30 re W n /Sh1 sh Q\n{KEEP}"))
    }
    .build();
    assert_eq!(apply(&partly, &marked(&partly, &[BOX])).map(|_| ()), Err(ApplyError::Refused(Refusal::ShadingUnderMark)));
    let axial =
        b"<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 200 0] /Function << /FunctionType 2 /Domain [0 1] /C0 [0.8] /C1 [1] /N 1 >> >>"
            .to_vec();
    let gradient = Spec {
        res: "/Shading << /Sh1 6 0 R >>".into(),
        extras: vec![axial],
        ..Spec::new(&format!("q 0 140 200 30 re W n /Sh1 sh Q\n{LINE}\n{KEEP}"))
    }
    .build();
    let applied = apply(&gradient, &marked(&gradient, &[BOX])).unwrap();
    assert!(found(&applied.bytes, SECRET).is_empty());
}

#[test]
fn the_self_check_reads_no_path_point_under_a_mark() {
    // R6b-54 N1: the output read again: a point of a path under an applied area (one the
    // redactor's cut would never leave) fails the self-check.
    let base = Spec::new(&format!("{LINE}\n{KEEP}")).build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert_eq!(apply::left_under_marks(&applied.bytes, &applied.facts.areas, applied.facts.overhang), Ok(()));
    let mut doc = pdfcraft_cos::Document::open(Arc::new(applied.bytes.clone())).unwrap();
    let page = pdfcraft_model::pages(&doc).swap_remove(0);
    let mut list = match page.dict.get(b"Contents").map(|c| (*doc.resolve(c)).clone()) {
        Some(pdfcraft_cos::Object::Array(items)) => items,
        Some(other) => vec![other],
        None => Vec::new(),
    };
    let added =
        doc.add(pdfcraft_cos::Object::Stream(pdfcraft_cos::Stream::from_raw(pdfcraft_cos::Dict::new(), b"0 g 50 150 m 51 151 l S\n".to_vec())));
    list.insert(0, pdfcraft_cos::Object::Ref(added));
    doc.update_dict(page.obj, |d| d.set(b"Contents".to_vec(), pdfcraft_cos::Object::Array(list))).unwrap();
    let forged = pdfcraft_cos::write_full(&doc, &pdfcraft_cos::SaveOptions { mod_date: None, ..Default::default() }).unwrap();
    assert!(matches!(apply::left_under_marks(&forged, &applied.facts.areas, applied.facts.overhang), Err(ApplyError::SelfCheck(_))));
}

// ── Audit (follow-up 9, coordinator's scan of operators.rs): the geometry self-check ─────────

/// `applied` written again with `content` drawn first on page 1, in a stream tagged `tag`
/// (`/PCMark`, the redactor's box tag when given), with `extra` resources.
fn forge_page(applied: &[u8], content: &[u8], tag: Option<&str>, form: Option<&[u8]>) -> Vec<u8> {
    let mut doc = pdfcraft_cos::Document::open(Arc::new(applied.to_vec())).unwrap();
    let page = pdfcraft_model::pages(&doc).swap_remove(0);
    let mut list = match page.dict.get(b"Contents").map(|c| (*doc.resolve(c)).clone()) {
        Some(pdfcraft_cos::Object::Array(items)) => items,
        Some(other) => vec![other],
        None => Vec::new(),
    };
    let mut dict = pdfcraft_cos::Dict::new();
    if let Some(tag) = tag {
        dict.set(b"PCMark".to_vec(), pdfcraft_cos::Object::name(tag));
    }
    let added = doc.add(pdfcraft_cos::Object::Stream(pdfcraft_cos::Stream::from_raw(dict, content.to_vec())));
    list.insert(0, pdfcraft_cos::Object::Ref(added));
    let mut resources = page.dict.get(b"Resources").and_then(|r| doc.resolve(r).as_dict().cloned()).unwrap_or_default();
    if let Some(form) = form {
        let mut form_dict = pdfcraft_cos::Dict::new();
        form_dict.set(b"Type".to_vec(), pdfcraft_cos::Object::name("XObject"));
        form_dict.set(b"Subtype".to_vec(), pdfcraft_cos::Object::name("Form"));
        form_dict.set(b"BBox".to_vec(), pdfcraft_cos::Object::Array([0, 0, 200, 300].map(pdfcraft_cos::Object::Int).to_vec()));
        let form = doc.add(pdfcraft_cos::Object::Stream(pdfcraft_cos::Stream::from_raw(form_dict, form.to_vec())));
        let mut xobjects = pdfcraft_cos::Dict::new();
        xobjects.set(b"Forged".to_vec(), pdfcraft_cos::Object::Ref(form));
        resources.set(b"XObject".to_vec(), pdfcraft_cos::Object::Dict(xobjects));
    }
    doc.update_dict(page.obj, |d| {
        d.set(b"Contents".to_vec(), pdfcraft_cos::Object::Array(list));
        d.set(b"Resources".to_vec(), pdfcraft_cos::Object::Dict(resources));
    })
    .unwrap();
    pdfcraft_cos::write_full(&doc, &pdfcraft_cos::SaveOptions { mod_date: None, ..Default::default() }).unwrap()
}

#[test]
fn the_geometry_self_check_reads_every_point_a_path_has_under_a_mark() {
    // Audit (operators.rs): the content check accepts the redactor's cut paths by their syntax;
    // where their points lie is the self-check's to judge, on the output `apply_marks` wrote.
    // Every way a forged output could keep a point under an applied area is refused: a
    // rectangle's corner, a curve's control point alone (its ends outside), a path built across
    // a change of the matrix, a path in a form drawn under the mark, and a path hidden in a
    // stream tagged as the redactor's boxes.
    let base = Spec::new(&format!("{LINE}\n{KEEP}")).build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert_eq!(apply::left_under_marks(&applied.bytes, &applied.facts.areas, applied.facts.overhang), Ok(()));
    for (case, content, tag, form) in [
        ("a rectangle", &b"0 g 40 150 4 4 re f\n"[..], None, None),
        ("a control point", &b"0 g 0 0 m 60 155 60 155 150 0 c S\n"[..], None, None),
        ("v's control point", &b"0 g 0 0 m 60 155 150 0 v S\n"[..], None, None),
        ("y's control point", &b"0 g 0 0 m 60 155 150 0 y S\n"[..], None, None),
        ("across a matrix", &b"q 0 g 0 0 m 1 0 0 1 50 150 cm 1 1 l S Q\n"[..], None, None),
        ("in a form", &b"q /Forged Do Q\n"[..], None, Some(&b"0 g 50 150 m 51 151 l S"[..])),
        ("as the boxes", &b"q 0 g 50 150 m 51 151 l S Q\n"[..], Some("Redaction"), None),
        ("a box elsewhere", &b"q 0 0 0 rg 150 10 20 20 re f Q\n"[..], Some("Redaction"), None),
    ] {
        let forged = forge_page(&applied.bytes, content, tag, form);
        assert!(matches!(apply::left_under_marks(&forged, &applied.facts.areas, applied.facts.overhang), Err(ApplyError::SelfCheck(_))), "{case}");
    }
}

// ── R6b-52, R6b-54 N3, N4, N5, N7, N8: copies of the removed words ─────────────────────────────

/// A page showing `line` (then "Keep"), with a bookmark titled `title` and document information
/// whose author is `author`.
fn with_copies(line: &str, title: &str, author: &str) -> Vec<u8> {
    Spec {
        cat: "/Outlines 6 0 R".into(),
        trailer: "/Info 8 0 R".into(),
        extras: vec![
            b"<< /Type /Outlines /First 7 0 R /Last 7 0 R /Count 1 >>".to_vec(),
            format!("<< /Title ({title}) /Parent 6 0 R /Dest [3 0 R /Fit] >>").into_bytes(),
            format!("<< /Author ({author}) /Title (Contract) >>").into_bytes(),
        ],
        ..Spec::new(&format!("{line}\n{KEEP}"))
    }
    .build()
}

#[test]
fn copies_of_short_names_kerned_words_and_accented_words_are_cleared() {
    // R6b-52, R6b-54 N5 (redaction re-review N5): the removed text read as an extractor reads it
    // (words apart at TJ gaps, glyph by glyph shows joined), folded (NFKC, diacritics, case) on
    // both sides; tokens of three letters or more, but function words, and every phrase.
    let wide = [[16.0, 144.0, 190.0, 166.0]];
    for (case, line, title, author, gone) in [
        ("a short name", "BT /F1 12 Tf 20 150 Td (Andi) Tj ET", "Agreement with Andi", "Andi", "Andi"),
        ("a title and a name", "BT /F1 12 Tf 20 150 Td (Mr Andi) Tj ET", "Andi", "Andi", "Andi"),
        ("two names", "BT /F1 12 Tf 20 150 Td (Andi Wijaya) Tj ET", "Andi", "Andi", "Andi"),
        ("kerned words", "BT /F1 12 Tf 20 150 Td [(Andi) -333 (Wijaya)] TJ ET", "Andi Wijaya", "Andi Wijaya", "Andi"),
        (
            "a glyph a show",
            "BT /F1 12 Tf 20 150 Td (W) Tj 11 0 Td (i) Tj 3 0 Td (j) Tj 3 0 Td (a) Tj 7 0 Td (y) Tj 6 0 Td (a) Tj ET",
            "Wijaya",
            "Wijaya",
            "Wijaya",
        ),
        ("a company", "BT /F1 12 Tf 20 150 Td (PT Budi SA) Tj ET", "PT Budi SA", "Budi", "Budi"),
        ("accents", "BT /F1 12 Tf 20 150 Td (Jos\\351 Garc\\355a) Tj ET", "Jose Garcia", "Jose", "Jose"),
    ] {
        let base = with_copies(line, title, author);
        let applied = apply(&base, &marked(&base, &wide)).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert!(found(&applied.bytes, gone).is_empty(), "{case}: {:?}", found(&applied.bytes, gone));
        assert_eq!((applied.facts.bookmarks_cleared, applied.facts.information_cleared), (1, 1), "{case}: {:?}", applied.facts);
    }
    // A function word alone clears no unrelated title, nor do two letters.
    for (line, title) in [("BT /F1 12 Tf 20 150 Td (the) Tj ET", "Other terms"), ("BT /F1 12 Tf 20 150 Td (AB) Tj ET", "AB Holdings")] {
        let base = with_copies(line, title, "Someone");
        let applied = apply(&base, &marked(&base, &wide)).unwrap();
        assert!(!found(&applied.bytes, title).is_empty(), "{title}");
        assert_eq!(applied.facts.bookmarks_cleared, 0);
    }
}

#[test]
fn text_without_unicode_under_a_mark_is_refused_where_copies_could_hold_it() {
    // R6b-54 N5 (`copies-no-unicode`): removed glyphs that read as nothing cannot be looked for
    // in the bookmarks and document information: refused by its reason.
    let mut spec = Spec {
        fonts: "/F2 9 0 R".into(),
        cat: "/Outlines 6 0 R".into(),
        trailer: "/Info 8 0 R".into(),
        extras: vec![
            b"<< /Type /Outlines /First 7 0 R /Last 7 0 R /Count 1 >>".to_vec(),
            b"<< /Title (SECRETWORD) /Parent 6 0 R /Dest [3 0 R /Fit] >>".to_vec(),
            b"<< /Author (SECRETWORD) >>".to_vec(),
            format!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type /Encoding /Differences [65 /g1 /g2 /g3 /g4 /g5 /g6 /g7 /g8 /g9 /g10] >> /FirstChar 32 /LastChar 126 /Widths [{HELV_WIDTHS}] >>").into_bytes(),
        ],
        ..Spec::new(&format!("BT /F2 12 Tf 20 150 Td (ABCDEFGHIJ) Tj ET\n{KEEP}"))
    };
    let base = spec.build();
    assert_eq!(apply(&base, &marked(&base, &[BOX])).map(|_| ()), Err(ApplyError::Refused(Refusal::UnreadableMarkedText)));
    // Without copies anywhere, it is applied.
    spec.cat.clear();
    spec.trailer.clear();
    let base = spec.build();
    assert!(apply(&base, &marked(&base, &[BOX])).is_ok());
}

#[test]
fn a_destination_named_by_a_hex_string_follows_its_renamed_name() {
    // R6b-54 N3 (`dest-tree-hex-link`): names matched by their bytes (a hex string alike), the
    // tree written again in order, and every link still going where it went.
    let base = Spec {
        cat: "/Names << /Dests 6 0 R >>".into(),
        page: "/Annots [9 0 R]".into(),
        extras: vec![
            b"<< /Kids [7 0 R 8 0 R] >>".to_vec(),
            b"<< /Limits [(Alpha) (SECRETWORD)] /Names [(Alpha) [3 0 R /Fit] (SECRETWORD) [3 0 R /Fit]] >>".to_vec(),
            b"<< /Limits [(Zeta) (Zulu)] /Names [(Zeta) [3 0 R /Fit] (Zulu) [3 0 R /Fit]] >>".to_vec(),
            b"<< /Type /Annot /Subtype /Link /Rect [150 10 170 30] /Border [0 0 0] /Dest <534543524554574F5244> >>".to_vec(),
        ],
        ..Spec::new(&format!("{LINE}\n{KEEP}"))
    }
    .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert!(found(&applied.bytes, SECRET).is_empty(), "{:?}", found(&applied.bytes, SECRET));
    assert_eq!((applied.facts.destinations_renamed, applied.inspection.destinations.renamed), (1, 1));
    // A fresh name is no name already in use.
    let base = Spec {
        cat: "/Dests << /SECRETWORD [3 0 R /Fit] /PCRedactedDest1 [3 0 R /FitH 100] >>".into(),
        page: "/Annots [6 0 R 7 0 R]".into(),
        extras: vec![
            b"<< /Type /Annot /Subtype /Link /Rect [150 10 170 30] /Border [0 0 0] /Dest /SECRETWORD >>".to_vec(),
            b"<< /Type /Annot /Subtype /Link /Rect [150 40 170 60] /Border [0 0 0] /Dest /PCRedactedDest1 >>".to_vec(),
        ],
        ..Spec::new(&format!("{LINE}\n{KEEP}"))
    }
    .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert!(found(&applied.bytes, SECRET).is_empty());
}

#[test]
fn the_xfa_copy_of_a_field_goes_with_any_redaction() {
    // R6b-54 N4 (`xfa-datasets`): a removed field's value stays in the XFA datasets: any applied
    // redaction removes /XFA (reported as a copy cleared).
    let base = Spec {
        cat: "/AcroForm << /Fields [6 0 R] /DA (/Helv 0 Tf 0 g) /XFA [(datasets) 8 0 R] >>".into(),
        page: "/Annots [6 0 R]".into(),
        extras: vec![
            b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (f1) /V (FIELDVALUE) /Rect [20 146 100 164] /P 3 0 R /AP << /N 7 0 R >> >>".to_vec(),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 80 20] /Resources << /Font << /F1 5 0 R >> >>", b"/Tx BMC BT /F1 10 Tf 2 5 Td (FIELDVALUE) Tj ET EMC"),
            stream("", b"<xfa:datasets xmlns:xfa='http://www.xfa.org/schema/xfa-data/1.0/'><xfa:data><form><f1>FIELDVALUE</f1></form></xfa:data></xfa:datasets>"),
        ],
        ..Spec::new(&format!("{LINE}\n{KEEP}"))
    }
    .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert!(found(&applied.bytes, "FIELDVALUE").is_empty(), "{:?}", found(&applied.bytes, "FIELDVALUE"));
    assert_eq!(applied.facts.xfa_removed, 1);
}

#[test]
fn xmp_and_other_copies_holding_a_removed_word_are_cleared() {
    // R6b-54 N7, N8 (`xmp-on-image-elsewhere`, `other-copies`): an image's XMP elsewhere, a
    // comment's text and author, a layer's name and a page-label prefix holding the removed word.
    let pixel = stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Metadata 7 0 R", &[7]);
    let xmp = stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta>SECRETWORD</x:xmpmeta>");
    let base = Spec {
        res: "/XObject << /Im1 6 0 R >>".into(),
        extras: vec![pixel, xmp],
        ..Spec::new(&format!("{LINE}\nq 10 0 0 10 150 20 cm /Im1 Do Q\n{KEEP}"))
    }
    .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert!(found(&applied.bytes, SECRET).is_empty(), "{:?}", found(&applied.bytes, SECRET));
    let base = Spec {
        res: "/Properties << /L1 6 0 R >>".into(),
        cat: "/OCProperties << /OCGs [6 0 R] /D << /ON [6 0 R] >> >> /PageLabels << /Nums [0 << /S /D /P (SECRETWORD-) >>] >>".into(),
        page: "/Annots [7 0 R]".into(),
        extras: vec![
            b"<< /Type /OCG /Name (SECRETWORD layer) >>".to_vec(),
            b"<< /Type /Annot /Subtype /Text /Rect [150 10 170 30] /T (SECRETWORD) /Contents (about SECRETWORD) >>".to_vec(),
        ],
        ..Spec::new(&format!("/OC /L1 BDC {KEEP} EMC\n{LINE}"))
    }
    .build();
    let applied = apply(&base, &marked(&base, &[BOX])).unwrap();
    assert!(found(&applied.bytes, SECRET).is_empty(), "{:?}", found(&applied.bytes, SECRET));
    let facts = &applied.facts;
    assert_eq!((facts.annotation_texts_cleared, facts.layer_names_cleared, facts.label_prefixes_cleared), (1, 1, 1), "{facts:?}");
}

#[test]
fn a_stream_coded_with_a_filter_not_read_to_its_end_is_only_the_base_s() {
    // R6b-53: an appended update (or the server's output) may bring a stream coded with LZW,
    // JPX, JBIG2, CCITT or Crypt only byte for byte as the base has it: past what is decoded,
    // anything could be held.
    let stamp = |metadata: Vec<u8>| {
        hand_update(&[
            page_one("[8 0 R 12 0 R]"),
            (12, b"<< /Type /Annot /Subtype /Stamp /Rect [100 100 140 120] /P 3 0 R /AP << /N 11 0 R >> >>".to_vec()),
            (11, stream(&format!("{STAMP_AP} /Metadata 13 0 R"), b"0 0 1 rg 0 0 40 20 re f")),
            (13, metadata),
        ])
    };
    for filter in ["/LZWDecode", "/JPXDecode", "/JBIG2Decode", "/CCITTFaxDecode", "/Crypt", "[/FlateDecode /LZWDecode]"] {
        let (base, edited) = stamp(stream(&format!("/Type /Metadata /Subtype /XML /Filter {filter}"), b"\x80\x0b\x60\x50SECRETWORD"));
        assert!(matches!(inspect(&base, &edited, InspectLimits::default()), Err(InspectError::Edited(_))), "{filter}");
    }
    let (base, edited) = stamp(stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta/>"));
    assert_eq!(inspect(&base, &edited, InspectLimits::default()).unwrap().annotations.stamps, 1);
}
