//! Embed build: conformance tests of unusual but valid, or malformed, content (Ruling R6b-20).
//!
//! Content hayro stops reading before its end (an operator after which it gives up) fails the
//! inspection: what follows would be checked by nothing, though another viewer may draw it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

// ── files ───────────────────────────────────────────────────────────────────────────────────

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// A 200 × 300 page: catalog 1 (with `catalog` entries), page tree 2, the page 3 (with `page`
/// entries) drawing `content` (object 4) with `resources` (a resource dictionary's entries, plus
/// Helvetica as `/F1`), then `objects` from 5.
fn page_file(catalog: &str, page: &str, content: &str, resources: &str, objects: &[Vec<u8>]) -> Vec<u8> {
    let mut bodies = vec![
        format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>").into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> {resources} >> {page} >>"
        )
        .into_bytes(),
        stream("", content.as_bytes()),
    ];
    bodies.extend(objects.iter().cloned());
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

/// A word the edited files redact, and the box that covers it (from the placement tests).
const WORD: &str = "BT /F1 12 Tf 150 20 Td (X) Tj ET";
const WORD_BOX: &str = "0 g 146 14 16 18 re f";

/// A word in Helvetica at (`x`, `y`).
fn text(x: u32, y: u32, word: &str) -> String {
    format!("BT /F1 12 Tf {x} {y} Td ({word}) Tj ET")
}

/// A Type 3 font whose glyph "a" (code 97) is drawn by `procedure` (object `procedure_object`).
fn type3_font(procedure_object: u32) -> Vec<u8> {
    format!(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 500 700] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a {procedure_object} 0 R >> \
         /Encoding << /Type /Encoding /Differences [97 /a] >> /FirstChar 97 /LastChar 97 /Widths [500] /Resources << >> >>"
    )
    .into_bytes()
}

/// The glyph "a": a black box over its advance.
const GLYPH: &str = "500 0 0 0 500 700 d1 0 0 500 700 re f";

/// A form XObject drawing `content`, its box `bbox`.
fn form(bbox: &str, content: &str) -> Vec<u8> {
    stream(&format!("/Type /XObject /Subtype /Form /BBox [{bbox}]"), content.as_bytes())
}

fn run(base: &[u8], edited: &[u8]) -> Result<Inspection, InspectError> {
    inspect(base, edited, InspectLimits::default())
}

fn edited_error(result: Result<Inspection, InspectError>) -> String {
    match result {
        Err(InspectError::Edited(message)) => message,
        other => panic!("expected Err(Edited), got {other:?}"),
    }
}

// ── (a) content read to its end ─────────────────────────────────────────────────────────────

#[test]
fn content_after_an_operator_that_ends_the_reading_is_refused() {
    // Each operator ends hayro's reading of the stream it is in; the word after it would be drawn
    // by a viewer that reads on, and checked by nothing.
    let stops = [("a path operator without its operands", "10 m"), ("more operands than the reader keeps", "1 2 3 4 5 6 7 8 9 10 11 12 sc")];
    let secret = text(20, 200, "Secret");
    let base = page_file("", "", &format!("{}\n{WORD}", text(20, 150, "Alpha")), "", &[]);
    for (case, stop) in stops {
        // In the page's own content.
        let edited = page_file("", "", &format!("{}\n{WORD_BOX}\n{stop}\n{secret}", text(20, 150, "Alpha")), "", &[]);
        let message = edited_error(run(&base, &edited));
        assert!(message.contains("stopped reading"), "{case}: {message}");
        // In a form the page draws.
        let in_form = |content: &str, page: &str| page_file("", "", page, "/XObject << /Fm 5 0 R >>", &[form("0 0 200 300", content)]);
        let base = in_form(&text(20, 150, "Alpha"), &format!("/Fm Do\n{WORD}"));
        let edited = in_form(&format!("{}\n{stop}\n{secret}", text(20, 150, "Alpha")), &format!("/Fm Do\n{WORD_BOX}"));
        let message = edited_error(run(&base, &edited));
        assert!(message.contains("stopped reading"), "{case}, in a form: {message}");
    }
    // In a Type 3 glyph procedure (read whenever the glyph is drawn).
    let with_glyph = |procedure: &str, word: &str| {
        page_file(
            "",
            "",
            &format!("BT /T3 20 Tf 30 150 Td (aa) Tj ET\n{word}"),
            "/Font << /T3 5 0 R >>",
            &[type3_font(6), stream("", procedure.as_bytes())],
        )
    };
    let base = with_glyph(GLYPH, WORD);
    let edited = with_glyph(&format!("{GLYPH} 10 m 0 0 500 700 re f"), WORD_BOX);
    let message = edited_error(run(&base, &edited));
    assert!(message.contains("stopped reading"), "in a glyph procedure: {message}");
    // The same files read to their end are compared as usual.
    let edited = page_file("", "", &format!("{}\n{WORD_BOX}", text(20, 150, "Alpha")), "", &[]);
    let inspection = run(&base_of_alpha(), &edited).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0);
    assert_eq!(inspection.redaction_areas.len(), 1);
}

fn base_of_alpha() -> Vec<u8> {
    page_file("", "", &format!("{}\n{WORD}", text(20, 150, "Alpha")), "", &[])
}

#[test]
fn a_base_whose_content_stops_early_is_the_base_s_problem() {
    let base = page_file("", "", &format!("{}\n10 m\n{}", text(20, 150, "Alpha"), text(20, 200, "Beta")), "", &[]);
    assert!(matches!(run(&base, &base), Err(InspectError::Base(message)) if message.contains("stopped reading")));
}
