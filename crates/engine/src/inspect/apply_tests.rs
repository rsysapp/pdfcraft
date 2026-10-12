//! Follow-up 8 (R6b-49, P6-32, P6-33, P6-35): marks applied on the server.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use super::*;
use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

/// A file of `pages` pages (200 × 300), page `i` drawing `content(i)`, objects as listed after.
fn file(contents: &[&str], extra: &[Vec<u8>]) -> Vec<u8> {
    let first = 3 + extra.len();
    let kids: Vec<String> = (0..contents.len()).map(|i| format!("{} 0 R", first + 2 * i)).collect();
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {} /MediaBox [0 0 200 300] >>", kids.join(" "), contents.len()).into_bytes(),
    ];
    bodies.extend(extra.iter().cloned());
    for (i, content) in contents.iter().enumerate() {
        bodies.push(
            format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 {HELVETICA} >> >> >>", first + 2 * i + 1).into_bytes(),
        );
        bodies.push(stream("", content.as_bytes()));
    }
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
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", bodies.len() + 1).as_bytes());
    out
}

/// The embed build's session: keeps the document information, marks redactions only.
fn marking_session(base: &[u8], edits: Vec<Edit>) -> Vec<u8> {
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    session.set_redaction_marks_only(true);
    let id = session.open("base.pdf", None, Arc::new(base.to_vec()), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

fn annotation(page: usize, shape: Shape) -> Edit {
    Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: String::new(), author: "Sari".into() })
}

fn mark(page: usize, quads: &[[f64; 4]]) -> Edit {
    annotation(page, Shape::Redact { quads: quads.iter().map(|q| rect_quad(*q)).collect(), overlay: String::new(), look: Default::default() })
}

fn holds(bytes: &[u8], word: &[u8]) -> bool {
    let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
    let mut all = bytes.to_vec();
    for num in doc.object_numbers() {
        if let Ok(object) = doc.try_get(num)
            && let pdfcraft_cos::Object::Stream(stream) = &*object
            && let Ok(data) = stream.decoded()
        {
            all.extend_from_slice(&data);
        }
    }
    all.windows(word.len()).any(|window| window == word)
}

const LINES: &str = "BT /F1 12 Tf 20 200 Td (SECRET one) Tj ET\nBT /F1 12 Tf 20 180 Td (SECRET two) Tj ET\nBT /F1 12 Tf 20 100 Td (Public) Tj ET";

#[test]
fn marks_are_applied_on_the_server_and_checked_against_the_edited_file() {
    // R6b-49, P6-32: the marks of an appended update are applied to the edited document; the
    // output is written whole and canonically; its own inspection against the edited document
    // reports only the applied redaction, even beside a rotation and a stamp in the same save.
    let base = file(&[LINES, "BT /F1 12 Tf 20 100 Td (Page two) Tj ET"], &[]);
    let edited = marking_session(
        &base,
        vec![
            Edit::RotatePages { pages: vec![1], degrees: 90 },
            annotation(1, Shape::Stamp { rect: [20.0, 20.0, 120.0, 60.0], stamp: pdfcraft_annot::StampKind::Approved, by: None }),
            mark(0, &[[16.0, 196.0, 100.0, 214.0]]),
        ],
    );
    assert!(edited.starts_with(&base));
    let applied = apply_marks(&base, &edited, InspectLimits::default()).unwrap();
    assert!(!holds(&applied.bytes, b"SECRET one"));
    assert!(holds(&applied.bytes, b"SECRET two") && holds(&applied.bytes, b"Public"));
    let inspection = &applied.inspection;
    assert_eq!(inspection.pages, PageChanges { before: 2, after: 2, ..PageChanges::default() }, "{inspection:?}");
    assert_eq!(inspection.annotations, AnnotationChanges { redactions_applied: 1, ..AnnotationChanges::default() }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!(applied.facts.marks, 1);
    // The header, and a fresh identifier of the writer's length.
    assert!(applied.bytes.starts_with(b"%PDF-1."));
    let output = pdfcraft_cos::Document::open(Arc::new(applied.bytes.clone())).unwrap();
    assert!(matches!(output.trailer().get(b"ID"), Some(pdfcraft_cos::Object::Array(id)) if id.len() == 2));
}

#[test]
fn a_mark_over_two_lines_is_applied_quad_by_quad() {
    // P6-35: one area per mark quadrilateral, in the page's default user space, each applied
    // within a point; nothing filled beyond them.
    let base = file(&[LINES], &[]);
    let quads = [[16.0, 196.0, 100.0, 214.0], [16.0, 176.0, 100.0, 194.0]];
    let edited = marking_session(&base, vec![mark(0, &quads)]);
    let before = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!(before.marked_areas, quads.iter().map(|q| (1, q.map(|v| v as f32))).collect::<Vec<_>>());
    let applied = apply_marks(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!(applied.facts.areas, before.marked_areas);
    assert_eq!(applied.inspection.applied_areas.len(), 2, "{:?}", applied.inspection);
    for ((_, applied), (_, quad)) in applied.inspection.applied_areas.iter().zip(&before.marked_areas) {
        assert!(applied.iter().zip(quad).all(|(a, b)| (a - b).abs() <= 1.0), "{applied:?} {quad:?}");
    }
    assert!(!holds(&applied.bytes, b"SECRET"));
}

#[test]
fn what_is_no_appended_update_with_marks_is_not_applied() {
    let base = file(&[LINES], &[]);
    let plain = marking_session(&base, vec![Edit::RotatePages { pages: vec![0], degrees: 90 }]);
    assert_eq!(apply_marks(&base, &plain, InspectLimits::default()), Err(ApplyError::NothingToApply));
    let rewritten = file(&["BT /F1 12 Tf 20 100 Td (Public) Tj ET"], &[]);
    assert!(matches!(apply_marks(&base, &rewritten, InspectLimits::default()), Err(ApplyError::Inspect(_))));
}

#[test]
fn every_content_stream_the_output_brings_is_written_canonically() {
    let base = file(&[LINES], &[]);
    let edited = marking_session(&base, vec![mark(0, &[[16.0, 196.0, 100.0, 214.0]])]);
    let applied = apply_marks(&base, &edited, InspectLimits::default()).unwrap();
    let doc = pdfcraft_cos::Document::open(Arc::new(applied.bytes)).unwrap();
    let mut budget = Budget::new(Limits::default());
    for page in pdfcraft_model::pages(&doc) {
        let list = match page.dict.get(b"Contents").map(|c| (*doc.resolve(c)).clone()) {
            Some(pdfcraft_cos::Object::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        for item in list {
            if let pdfcraft_cos::Object::Stream(stream) = &*doc.resolve(&item) {
                let data = stream.decoded().unwrap();
                if data.starts_with(b"BT") || data.starts_with(b"q") {
                    let canonical = operators::canonical(&data, &mut budget).unwrap();
                    assert!(canonical == data || data == b"BT /F1 12 Tf 20 180 Td (SECRET two) Tj ET\n", "{}", String::from_utf8_lossy(&data));
                }
            }
        }
    }
}

/// [`file`] with `catalog` entries in the catalog.
fn file_with_catalog(contents: &[&str], catalog: &str, extra: &[Vec<u8>]) -> Vec<u8> {
    let plain = file(contents, extra);
    let text = String::from_utf8_lossy(&plain).into_owned();
    // Rebuilt rather than patched: the catalog's length changes every offset.
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let doc = pdfcraft_cos::Document::open(Arc::new(plain.clone())).unwrap();
    let _ = text;
    for num in 1..=doc.object_numbers().len() as u32 {
        let start =
            plain.windows(format!("\n{num} 0 obj\n").len()).position(|w| w == format!("\n{num} 0 obj\n").as_bytes()).map(|at| at + 1).unwrap_or(9);
        let body_start = start + format!("{num} 0 obj\n").len();
        let end = plain[body_start..].windows(8).position(|w| w == b"\nendobj\n").unwrap() + body_start;
        bodies.push(plain[body_start..end].to_vec());
    }
    bodies[0] = format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>").into_bytes();
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
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", bodies.len() + 1).as_bytes());
    out
}

#[test]
fn copies_of_the_removed_words_go_and_the_self_check_reports_them() {
    // R6b-50: a bookmark title and a named destination holding the removed word are cleared
    // and renamed; the self-check reports exactly those.
    let base = file_with_catalog(
        &[LINES],
        "/Outlines 3 0 R /Dests << /SECRET-one [5 0 R /Fit] >>",
        &[
            b"<< /Type /Outlines /First 4 0 R /Last 4 0 R /Count 1 >>".to_vec(),
            b"<< /Title (SECRET one clause) /Parent 3 0 R /Dest /SECRET-one >>".to_vec(),
        ],
    );
    let edited = marking_session(&base, vec![mark(0, &[[16.0, 196.0, 100.0, 214.0]])]);
    let applied = apply_marks(&base, &edited, InspectLimits::default()).unwrap();
    assert!(!holds(&applied.bytes, b"SECRET one"));
    assert_eq!((applied.facts.bookmarks_cleared, applied.facts.destinations_renamed), (1, 1), "{:?}", applied.facts);
    assert_eq!((applied.inspection.outlines.titles_changed, applied.inspection.destinations.renamed), (1, 1), "{:?}", applied.inspection);
}

#[test]
fn what_an_update_adds_under_its_marks_is_counted_and_goes_with_them() {
    // P6-35: an update's own stamps and drawings wholly under its marks are counted, and the
    // server's redaction removes them (reported as removed by the redaction, never plain
    // removals).
    let base = file(&[LINES], &[]);
    let under = Shape::Stamp { rect: [20.0, 198.0, 60.0, 212.0], stamp: pdfcraft_annot::StampKind::Approved, by: None };
    let reaching = Shape::Rectangle { rect: [90.0, 198.0, 150.0, 212.0] };
    let edited = marking_session(&base, vec![annotation(0, under), annotation(0, reaching.clone()), mark(0, &[[16.0, 196.0, 100.0, 214.0]])]);
    let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!((inspection.annotations.stamps_under_marks, inspection.annotations.drawings_under_marks), (1, 0), "{inspection:?}");
    // The rectangle reaches out of the mark: the redactor removes what reaches in, which is no
    // removal by the redaction (it was not wholly under the mark): the server refuses.
    assert!(matches!(apply_marks(&base, &edited, InspectLimits::default()), Err(ApplyError::SelfCheck(_))));
    let edited = marking_session(
        &base,
        vec![
            annotation(0, Shape::Stamp { rect: [20.0, 198.0, 60.0, 212.0], stamp: pdfcraft_annot::StampKind::Approved, by: None }),
            mark(0, &[[16.0, 196.0, 100.0, 214.0]]),
        ],
    );
    let applied = apply_marks(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!((applied.inspection.annotations.removed_by_redaction, applied.inspection.annotations.removed), (1, 0), "{:?}", applied.inspection);
}
