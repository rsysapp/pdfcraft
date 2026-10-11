//! Embed build: conformance tests of unusual but valid, or malformed, content.
//!
//! - Content hayro stops reading before its end (an operator after which it gives up) fails the
//!   inspection: what follows would be checked by nobody, though another viewer may draw it.
//! - Form XObjects drawn more than once, Type 3 fonts and annotation appearances are compared as
//!   content streams: by their decoded data where hayro decoded them, else by their encoded
//!   data; in a rewrite, a difference that does not lie wholly under the redaction boxes is a
//!   change outside redactions.
//! - Every image the content draws (as `pdfcraft_cos` reads it: directly, in forms and in tiling
//!   patterns) must be one hayro drew. hayro draws every optional-content layer, so content in a
//!   layer that is off is compared like any other.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

// ── files ───────────────────────────────────────────────────────────────────────────────────

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

fn flate_stream(dict: &str, data: &[u8]) -> Vec<u8> {
    stream(&format!("{dict} /Filter /FlateDecode"), &pdfcraft_cos::Stream::flate(pdfcraft_cos::Dict::new(), data).raw)
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

/// Append an update replacing object `num` with `body`.
fn update(base: &[u8], num: u32, body: &[u8]) -> Vec<u8> {
    let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
    let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let size = base.windows(6).rposition(|w| w == b"/Size ").unwrap();
    let size: u32 = std::str::from_utf8(&base[size + 6..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let mut out = base.to_vec();
    let offset = out.len();
    out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(b"\nendobj\n");
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \n{num} 1\n{offset:010} 00000 n \n").as_bytes());
    out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
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

/// A `width` × `height` grey texture, as a scan has.
fn texture(width: usize, height: usize) -> Vec<u8> {
    (0..width * height).map(|i| (96 + (i % width) * 7 % 61 + (i / width) * 3 % 37) as u8).collect()
}

/// An uncompressed grey image XObject with `extra` dictionary entries.
fn grey(width: usize, height: usize, pixels: &[u8], extra: &str) -> Vec<u8> {
    stream(&format!("/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 {extra}"), pixels)
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

// ── (b) content streams by their decoded content ────────────────────────────────────────────

#[test]
fn a_type3_glyph_procedure_changed_outside_any_box_is_a_change_outside_redactions() {
    let with_glyph = |procedure: Vec<u8>, word: &str| {
        page_file("", "", &format!("BT /T3 20 Tf 30 150 Td (aa) Tj ET\n{word}"), "/Font << /T3 5 0 R >>", &[type3_font(6), procedure])
    };
    let base = with_glyph(stream("", GLYPH.as_bytes()), WORD);
    // Drawing more far off the page: nothing shows, yet the glyph is not what it was.
    let changed = with_glyph(stream("", format!("{GLYPH} -9000 -9000 100 100 re f").as_bytes()), WORD_BOX);
    let inspection = run(&base, &changed).unwrap();
    assert!(!inspection.base_kept);
    // The word is redacted; the glyphs drawn with the procedure are where they were.
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    // The same procedure encoded otherwise is the same glyph.
    let recoded = with_glyph(flate_stream("", GLYPH.as_bytes()), WORD_BOX);
    let inspection = run(&base, &recoded).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!(inspection.redaction_areas.len(), 1);
}

#[test]
fn an_annotation_appearance_changed_in_a_rewrite_is_a_change_outside_redactions() {
    let annotated = |appearance: Vec<u8>, word: &str| {
        page_file(
            "",
            "/Annots [5 0 R]",
            &format!("{}\n{word}", text(20, 150, "Alpha")),
            "",
            &[b"<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /C [0 0 1] /AP << /N 6 0 R >> >>".to_vec(), appearance],
        )
    };
    let square = |colour: &str| format!("{colour} RG 2 2 56 56 re S");
    let appearance = |content: &str| stream("/Type /XObject /Subtype /Form /BBox [0 0 60 60]", content.as_bytes());
    let base = annotated(appearance(&square("0 0 1")), WORD);
    let changed = annotated(appearance(&square("1 0 0")), WORD_BOX);
    let inspection = run(&base, &changed).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    // The same appearance encoded otherwise: no second reader decodes an appearance, so it is
    // compared as encoded, and it changed.
    let recoded = annotated(flate_stream("/Type /XObject /Subtype /Form /BBox [0 0 60 60]", square("0 0 1").as_bytes()), WORD_BOX);
    let inspection = run(&base, &recoded).unwrap();
    assert_eq!(inspection.annotations, AnnotationChanges { drawings: 1, removed: 1, ..AnnotationChanges::default() }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    // Written again as it was, it is the same annotation.
    let same = annotated(appearance(&square("0 0 1")), WORD_BOX);
    let inspection = run(&base, &same).unwrap();
    assert_eq!(inspection.annotations, AnnotationChanges::default(), "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
}

#[test]
fn a_form_encoded_otherwise_with_the_same_content_is_unchanged() {
    let content = text(10, 10, "Alpha");
    let base = page_file("", "", &format!("/Fm Do\n{WORD}"), "/XObject << /Fm 5 0 R >>", &[form("0 0 200 300", &content)]);
    // An appended update writing the form again, compressed.
    let recoded = update(&base, 5, &flate_stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300]", content.as_bytes()));
    let inspection = run(&base, &recoded).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.content, ContentChanges::default(), "{inspection:?}");
    // A rewrite that redacts the word and compresses the form.
    let rewrite = page_file(
        "",
        "",
        &format!("/Fm Do\n{WORD_BOX}"),
        "/XObject << /Fm 5 0 R >>",
        &[flate_stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300]", content.as_bytes())],
    );
    let inspection = run(&base, &rewrite).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!(inspection.redaction_areas.len(), 1);
}

#[test]
fn a_form_whose_decoded_content_differs_outside_the_boxes_is_a_change_outside_redactions() {
    // Drawn twice: a change to the form changes both places.
    let with_form = |content: &str, word: &str| {
        page_file("", "", &format!("/Fm Do q 1 0 0 1 100 150 cm /Fm Do Q\n{word}"), "/XObject << /Fm 5 0 R >>", &[form("0 0 100 100", content)])
    };
    let base = with_form(&text(10, 10, "Alpha"), WORD);
    // Drawing outside the form's box: the box clips it away everywhere, yet the form changed.
    let changed = with_form(&format!("{} 0 g 500 500 10 10 re f", text(10, 10, "Alpha")), WORD_BOX);
    let inspection = run(&base, &changed).unwrap();
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    // A soft mask's group is a form too.
    let masked = |group: &str, word: &str| {
        page_file(
            "",
            "",
            &format!("q /GS1 gs 0 g 20 200 50 50 re f Q\n{word}"),
            "/ExtGState << /GS1 << /SMask << /S /Luminosity /G 5 0 R >> >> >>",
            &[stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /CS /DeviceGray >>", group.as_bytes())],
        )
    };
    let base = masked("1 g 0 0 200 300 re f", WORD);
    let changed = masked("1 g 0 0 200 300 re f 0 g 500 500 10 10 re f", WORD_BOX);
    let inspection = run(&base, &changed).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "a soft mask's group: {inspection:?}");
    // A form wholly under a box may change: the redactor rewrites what it draws there.
    let under = |content: &str, word: &str| {
        page_file(
            "",
            "",
            &format!("{}\nq 1 0 0 1 146 14 cm /Fm Do Q\n{word}", text(20, 150, "Alpha")),
            "/XObject << /Fm 5 0 R >>",
            &[form("0 0 16 18", content)],
        )
    };
    let base = under("BT /F1 12 Tf 4 6 Td (X) Tj ET", "");
    let redacted = under("", WORD_BOX);
    let inspection = run(&base, &redacted).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!(inspection.text.pages_with_text_removed, 1);
    assert_eq!(inspection.redaction_areas.len(), 1);
}

// ── (c) every image the content draws is drawn ──────────────────────────────────────────────

/// Optional content: group 6 (`/L` in the page's `/Properties`) in a configuration `config`.
fn layered(config: &str, content: &str, image: Vec<u8>, extra: &[Vec<u8>]) -> Vec<u8> {
    let mut objects = vec![image, b"<< /Type /OCG /Name (Layer) >>".to_vec()];
    objects.extend(extra.iter().cloned());
    // `/M`: the first extra object (a membership dictionary), when there is one.
    let membership = if extra.is_empty() { "" } else { "/M 7 0 R" };
    page_file(
        &format!("/OCProperties << /OCGs [6 0 R] /D << {config} >> >>"),
        "",
        content,
        &format!("/XObject << /Im1 5 0 R >> /Properties << /L 6 0 R {membership} >>"),
        &objects,
    )
}

const DRAW_IMAGE: &str = "q 100 0 0 100 50 100 cm /Im1 Do Q";

type LayerCase = (&'static str, &'static str, &'static str, &'static str, Vec<Vec<u8>>);

#[test]
fn an_image_in_optional_content_that_is_off_is_drawn_and_compared() {
    let pixels = texture(16, 16);
    let mut changed = pixels.clone();
    changed[0] ^= 0x40;
    let membership = b"<< /Type /OCMD /OCGs [6 0 R] /P /AllOn /VE [/Not 6 0 R] >>".to_vec();
    // (case, configuration, marked content around the image, the image's /OC, extra objects)
    let cases: [LayerCase; 6] = [
        ("marked as off", "/OFF [6 0 R]", "/OC /L BDC", "", vec![]),
        ("off by the base state", "/BaseState /OFF", "/OC /L BDC", "", vec![]),
        ("an image whose /OC is off", "/OFF [6 0 R]", "", "/OC 6 0 R", vec![]),
        // Readers disagree on these: hayro alone hid them before it drew every layer.
        ("a group named by a tag other than /OC", "/OFF [6 0 R]", "/Span /L BDC", "", vec![]),
        ("a membership dictionary with a visibility expression", "/OFF [6 0 R]", "/OC /M BDC", "", vec![membership]),
        ("a group with an automatic state", "/OFF [6 0 R] /AS [<< /Event /View /OCGs [6 0 R] /Category [/View] >>]", "/OC /L BDC", "", vec![]),
    ];
    for (case, config, begin, oc, extra) in cases {
        let (open, close) = if begin.is_empty() { ("", "") } else { (begin, "EMC") };
        let file = |pixels: &[u8], word: &str| layered(config, &format!("{open} {DRAW_IMAGE} {close}\n{word}"), grey(16, 16, pixels, oc), &extra);
        let base = file(&pixels, WORD);
        let inspection = run(&base, &base).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(inspection.content, ContentChanges::default(), "{case}");
        // A viewer's Layers panel shows it: compared like any image.
        let inspection = run(&base, &file(&changed, WORD_BOX)).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{case}: {inspection:?}");
    }
}

#[test]
fn an_image_the_content_draws_that_hayro_does_not_is_refused() {
    let pixels = texture(16, 16);
    // An image in a tiling pattern's cell that hayro never paints (nothing is filled with it).
    let pattern = stream(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << /XObject << /Im 5 0 R >> >>",
        b"10 0 0 10 0 0 cm /Im Do",
    );
    let with_pattern = |paint: &str, word: &str| {
        page_file("", "", &format!("/Pattern cs /P1 scn {paint}\n{word}"), "/Pattern << /P1 6 0 R >>", &[grey(16, 16, &pixels, ""), pattern.clone()])
    };
    let base = with_pattern("20 20 50 50 re f", WORD);
    assert!(run(&base, &base).is_ok());
    let message = edited_error(run(&base, &with_pattern("", WORD_BOX)));
    assert!(message.contains("did not draw every use of image"), "a pattern never painted: {message}");
}
