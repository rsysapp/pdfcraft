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

// ── follow-up: streams keyed by their resolved dictionary and their data ────────────────────

/// Append one update holding `objects` (number, body) to `base`, with `/Size` `size`.
fn update_objects(base: &[u8], objects: &[(u32, Vec<u8>)], size: u32) -> Vec<u8> {
    let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
    let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let mut out = base.to_vec();
    let mut rows = Vec::new();
    for (num, body) in objects {
        rows.push((*num, out.len()));
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    rows.sort_unstable();
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (num, offset) in rows {
        out.extend_from_slice(format!("{num} 1\n{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

/// `data` encoded with RunLengthDecode (literal runs only).
fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(128) {
        out.push((chunk.len() - 1) as u8);
        out.extend_from_slice(chunk);
    }
    out.push(128);
    out
}

/// The appearance a Square annotation (object 5) of the edited files shows: a blue square.
const SQUARE: &str = "0 0 1 RG 2 2 56 56 re S";

/// A page with a Square annotation (object 5) whose appearance dictionary is `appearances`
/// (naming streams 6 and 7), and `objects` from 6.
fn with_square(appearances: &str, objects: &[Vec<u8>]) -> Vec<u8> {
    let mut all = vec![format!("<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /C [0 0 1] /AP {appearances} >>").into_bytes()];
    all.extend(objects.iter().cloned());
    page_file("", "/Annots [5 0 R]", &text(20, 150, "Alpha"), "", &all)
}

fn appearance_stream(extra: &str, data: &[u8]) -> Vec<u8> {
    stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 60 60] {extra}"), data)
}

#[test]
fn an_indirect_filter_on_an_appearance_is_a_change() {
    // An appended update writes the appearance again, byte for byte, with `/Filter 9 0 R`
    // (RunLengthDecode): readers now decode it into other drawing (PDFium draws it black).
    for (case, appearances) in [("normal", "<< /N 6 0 R >>"), ("down", "<< /N 7 0 R /D 6 0 R >>"), ("rollover", "<< /N 7 0 R /R 6 0 R >>")] {
        let base = with_square(appearances, &[appearance_stream("", SQUARE.as_bytes()), appearance_stream("", SQUARE.as_bytes())]);
        let edited = update_objects(&base, &[(6, appearance_stream("/Filter 9 0 R", SQUARE.as_bytes())), (9, b"/RunLengthDecode".to_vec())], 10);
        let inspection = run(&base, &edited).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert!(inspection.base_kept);
        assert_eq!(inspection.annotations.changed, 1, "{case}: {inspection:?}");
    }
    // A form in an appearance's resources, changed the same way.
    let nested = |filter: &str| appearance_stream(&format!("/Resources << /XObject << /Inner 7 0 R >> >> {filter}"), b"/Inner Do");
    let base = with_square("<< /N 6 0 R >>", &[nested(""), appearance_stream("", SQUARE.as_bytes())]);
    let edited = update_objects(&base, &[(7, appearance_stream("/Filter 9 0 R", SQUARE.as_bytes())), (9, b"/RunLengthDecode".to_vec())], 10);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.annotations.changed, 1, "a form in an appearance's resources: {inspection:?}");
}

#[test]
fn indirect_decode_parameters_on_an_appearance_are_a_change() {
    // The same Flate data, now with `/DecodeParms 9 0 R` (a PNG predictor): another drawing.
    let flated = |extra: &str| {
        stream(
            &format!("/Type /XObject /Subtype /Form /BBox [0 0 60 60] /Filter /FlateDecode {extra}"),
            &pdfcraft_cos::Stream::flate(pdfcraft_cos::Dict::new(), SQUARE.as_bytes()).raw,
        )
    };
    let base = with_square("<< /N 6 0 R >>", &[flated(""), b"null".to_vec()]);
    let edited = update_objects(&base, &[(6, flated("/DecodeParms 9 0 R")), (9, b"<< /Predictor 12 /Columns 4 >>".to_vec())], 10);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.annotations.changed, 1, "{inspection:?}");
}

#[test]
fn a_form_encoded_with_an_indirect_filter_is_decoded_as_written_directly() {
    // `pdfcraft_cos` applies an indirect filter as hayro does: a form re-encoded with one draws
    // what it drew (it was taken undecoded, and the readers disagreed).
    let content = text(10, 10, "Alpha");
    let base = page_file("", "", &format!("/Fm Do\n{WORD}"), "/XObject << /Fm 5 0 R >>", &[form("0 0 200 300", &content)]);
    let encoded = stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Filter 9 0 R", &run_length(content.as_bytes()));
    let inspection = run(&base, &update_objects(&base, &[(5, encoded), (9, b"/RunLengthDecode".to_vec())], 10)).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.content, ContentChanges::default(), "{inspection:?}");
}

/// An appended update giving a stream hayro does not decode another filter: noticed as a change
/// (`count` of it), or refused where `pdfcraft_cos` reads the stream (scanning what it draws) and
/// cannot decode it so.
fn noticed(result: Result<Inspection, InspectError>, count: impl Fn(&Inspection) -> u32, case: &str) {
    match result {
        Ok(inspection) => assert_eq!(count(&inspection), 1, "{case}: {inspection:?}"),
        Err(InspectError::Edited(message)) => assert!(message.contains("cannot be decoded"), "{case}: {message}"),
        other => panic!("{case}: {other:?}"),
    }
}

#[test]
fn streams_hayro_does_not_decode_are_compared_as_encoded() {
    let filter = (9, b"/RunLengthDecode".to_vec());
    // A form-field widget's appearance.
    let widget = |appearance: Vec<u8>| {
        page_file(
            "/AcroForm << /Fields [5 0 R] >>",
            "/Annots [5 0 R]",
            &text(20, 150, "Alpha"),
            "",
            &[
                b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /V (Ann) /Rect [20 200 120 220] /P 3 0 R /AP << /N 6 0 R >> >>".to_vec(),
                appearance,
            ],
        )
    };
    let shows = b"BT /F1 12 Tf 2 5 Td (Ann) Tj ET";
    let base = widget(stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 100 20] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>",
        shows,
    ));
    let edited = update_objects(
        &base,
        &[
            (
                6,
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 100 20] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Filter 9 0 R",
                    shows,
                ),
            ),
            filter.clone(),
        ],
        10,
    );
    assert_eq!(run(&base, &edited).unwrap().form_fields_changed, 1, "a widget's appearance");
    // A Type 3 glyph procedure for a glyph no page draws ("b").
    let font = |b_object: u32| {
        format!(
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 500 700] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 6 0 R /b {b_object} 0 R >> \
             /Encoding << /Type /Encoding /Differences [97 /a /b] >> /FirstChar 97 /LastChar 98 /Widths [500 500] /Resources << >> >>"
        )
        .into_bytes()
    };
    let base = page_file(
        "",
        "",
        &format!("BT /T3 20 Tf 30 150 Td (aa) Tj ET\n{WORD}"),
        "/Font << /T3 5 0 R >>",
        &[font(7), stream("", GLYPH.as_bytes()), stream("", GLYPH.as_bytes())],
    );
    let edited = update_objects(&base, &[(7, stream("/Filter 9 0 R", GLYPH.as_bytes())), filter.clone()], 10);
    noticed(run(&base, &edited), |i| i.content.pages_with_content_changed, "a glyph procedure no page draws");
    // The same procedure validly encoded so: compared as encoded, it changed.
    let edited = update_objects(&base, &[(7, stream("/Filter 9 0 R", &run_length(GLYPH.as_bytes()))), filter.clone()], 10);
    assert_eq!(run(&base, &edited).unwrap().content.pages_with_content_changed, 1, "a glyph procedure no page draws, encoded");
    // A soft mask's group nothing is painted through.
    let masked = page_file(
        "",
        "",
        &format!("/GS1 gs\n{WORD}"),
        "/ExtGState << /GS1 << /SMask << /S /Luminosity /G 5 0 R >> >> >>",
        &[stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /CS /DeviceGray >>", b"1 g 0 0 200 300 re f")],
    );
    let edited = update_objects(
        &masked,
        &[
            (
                5,
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /CS /DeviceGray >> /Filter 9 0 R",
                    b"1 g 0 0 200 300 re f",
                ),
            ),
            filter.clone(),
        ],
        10,
    );
    noticed(run(&masked, &edited), |i| i.content.pages_with_content_changed, "an unpainted soft mask's group");
    let edited = update_objects(
        &masked,
        &[
            (
                5,
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /CS /DeviceGray >> /Filter 9 0 R",
                    &run_length(b"1 g 0 0 200 300 re f"),
                ),
            ),
            filter.clone(),
        ],
        10,
    );
    // hayro interprets a mask's group when the state sets it, painted through or not: decoded
    // by both readers alike, the group is compared by its decoded data, and it did not change.
    assert_eq!(run(&masked, &edited).unwrap().content.pages_with_content_changed, 0, "an unpainted soft mask's group, encoded");
    // A form in a layer that is off: drawn now, and decoded by both readers; the same bytes
    // under another filter are other content, or cannot be read.
    let layer = page_file(
        "/OCProperties << /OCGs [6 0 R] /D << /OFF [6 0 R] >> >>",
        "",
        &format!("/OC /L BDC /Fm Do EMC\n{WORD}"),
        "/XObject << /Fm 5 0 R >> /Properties << /L 6 0 R >>",
        &[form("0 0 200 300", &text(10, 10, "Alpha")), b"<< /Type /OCG /Name (Layer) >>".to_vec()],
    );
    let edited = update_objects(
        &layer,
        &[(5, stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Filter 9 0 R", text(10, 10, "Alpha").as_bytes())), filter],
        10,
    );
    noticed(run(&layer, &edited), |i| i.content.pages_with_content_changed, "a form in a layer that is off");
}

// ── follow-up: every layer is content ───────────────────────────────────────────────────────

/// A page drawing "Alpha" and `content`, with group 6 (`/L`) and membership dictionary 7 (`/M`)
/// in a configuration listing `listed` groups and saying `config`.
fn layer_page(listed: &str, config: &str, content: &str) -> Vec<u8> {
    page_file(
        &format!("/OCProperties << /OCGs [{listed}] /D << {config} >> >>"),
        "",
        &format!("{}\n{content}", text(20, 150, "Alpha")),
        "/Properties << /L 6 0 R /M 7 0 R >>",
        &[b"null".to_vec(), b"<< /Type /OCG /Name (Layer) >>".to_vec(), b"<< /Type /OCMD /OCGs [6 0 R] /P /AllOn /VE [/Not 6 0 R] >>".to_vec()],
    )
}

#[test]
fn text_added_where_hayro_alone_hid_it_is_seen() {
    // Each was accepted as a redaction of one word while it added "Secret": hayro hid it, other
    // readers show it (or show it with the layer on).
    let cases = [
        ("a group named by /Span, the group off", "6 0 R", "/OFF [6 0 R]", "/Span /L BDC"),
        ("a group off but not listed in /OCGs", "", "/OFF [6 0 R]", "/OC /L BDC"),
        ("a membership dictionary with a visibility expression", "6 0 R", "/OFF [6 0 R]", "/OC /M BDC"),
        ("a group with an automatic state", "6 0 R", "/OFF [6 0 R] /AS [<< /Event /View /OCGs [6 0 R] /Category [/View] >>]", "/OC /L BDC"),
    ];
    for (case, listed, config, begin) in cases {
        let base = layer_page(listed, config, WORD);
        assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default(), "{case}");
        let edited = layer_page(listed, config, &format!("{WORD_BOX}\n{begin} {} EMC", text(20, 200, "Secret")));
        let inspection = run(&base, &edited).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(inspection.text.pages_with_text_added, 1, "{case}: {inspection:?}");
        assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{case}: {inspection:?}");
    }
}

#[test]
fn text_added_in_a_layer_that_is_off_is_a_change_outside_redactions() {
    let base = layer_page("6 0 R", "/OFF [6 0 R]", WORD);
    let edited = layer_page("6 0 R", "/OFF [6 0 R]", &format!("{WORD_BOX}\n/OC /L BDC {} EMC", text(20, 200, "Secret")));
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.text.pages_with_text_added, 1, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    // Text in the layer redacted under a box is a redaction like any other.
    let base = layer_page("6 0 R", "/OFF [6 0 R]", &format!("/OC /L BDC {WORD} EMC"));
    let edited = layer_page("6 0 R", "/OFF [6 0 R]", WORD_BOX);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!(inspection.redaction_areas.len(), 1);
}

#[test]
fn an_image_in_a_group_off_but_not_listed_in_ocgs_is_compared() {
    let pixels = texture(16, 16);
    let mut changed = pixels.clone();
    changed[0] ^= 0x40;
    let file = |pixels: &[u8], word: &str| {
        page_file(
            "/OCProperties << /OCGs [] /D << /OFF [6 0 R] >> >>",
            "",
            &format!("/OC /L BDC {DRAW_IMAGE} EMC\n{word}"),
            "/XObject << /Im1 5 0 R >> /Properties << /L 6 0 R >>",
            &[grey(16, 16, pixels, ""), b"<< /Type /OCG /Name (Layer) >>".to_vec()],
        )
    };
    let base = file(&pixels, WORD);
    let inspection = run(&base, &file(&changed, WORD_BOX)).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
}

#[test]
fn a_rewrite_that_changes_which_layers_show_is_a_change_outside_redactions() {
    let base = layer_page("6 0 R", "", &format!("/OC /L BDC {} EMC\n{WORD}", text(20, 200, "Shown")));
    let edited = layer_page("6 0 R", "/OFF [6 0 R]", &format!("/OC /L BDC {} EMC\n{WORD_BOX}", text(20, 200, "Shown")));
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
}

// ── follow-up: Type 3 fonts with everything their procedures draw ───────────────────────────

/// A page showing "aa" in Type 3 font `/T3` (object 5, its glyph "a" drawn by object 6, its own
/// resources `resources`), then `word`; `objects` from 7, and page resources `page_resources`.
fn type3_page(resources: &str, procedure: &str, page_resources: &str, word: &str, objects: &[Vec<u8>]) -> Vec<u8> {
    let font = format!(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 500 700] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 6 0 R >> \
         /Encoding << /Type /Encoding /Differences [97 /a] >> /FirstChar 97 /LastChar 97 /Widths [500] {resources} >>"
    );
    let mut all = vec![font.into_bytes(), stream("", procedure.as_bytes())];
    all.extend(objects.iter().cloned());
    page_file("", "", &format!("BT /T3 20 Tf 30 150 Td (aa) Tj ET\n{word}"), &format!("/Font << /T3 5 0 R >> {page_resources}"), &all)
}

#[test]
fn a_type3_font_inside_a_glyph_procedure_is_keyed() {
    // Glyph "a" shows glyph "a" of an inner Type 3 font (object 7, procedure 8).
    let inner = |procedure: &str| [type3_font(8), stream("", procedure.as_bytes())];
    let outer = "500 0 d0 BT /U 1000 Tf 0 0 Td (a) Tj ET";
    let base = type3_page("/Resources << /Font << /U 7 0 R >> >>", outer, "", WORD, &inner(GLYPH));
    assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default());
    // The inner procedure draws more, off the page.
    let edited = type3_page("/Resources << /Font << /U 7 0 R >> >>", outer, "", WORD_BOX, &inner(&format!("{GLYPH} -9000 -9000 100 100 re f")));
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
}

#[test]
fn a_form_inside_a_glyph_procedure_is_keyed() {
    let procedure = "500 0 d0 /Fx Do";
    let with = |form_content: &str, word: &str| {
        type3_page("/Resources << /XObject << /Fx 7 0 R >> >>", procedure, "", word, &[form("0 0 500 700", form_content)])
    };
    let base = with("0 0 500 700 re f", WORD);
    assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default());
    // The form draws more outside its box (clipped away).
    let inspection = run(&base, &with("0 0 500 700 re f 900 900 10 10 re f", WORD_BOX)).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    // A font without resources draws with the page's: a form named there is keyed too.
    let from_page =
        |form_content: &str, word: &str| type3_page("", procedure, "/XObject << /Fx 7 0 R >>", word, &[form("0 0 500 700", form_content)]);
    let base = from_page("0 0 500 700 re f", WORD);
    assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default());
    let inspection = run(&base, &from_page("0 0 500 700 re f 900 900 10 10 re f", WORD_BOX)).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "the page's form: {inspection:?}");
}

#[test]
fn an_image_a_glyph_procedure_draws_is_keyed() {
    let pixels = texture(4, 4);
    let mut changed = pixels.clone();
    changed[0] ^= 0x40;
    // Drawn by glyph "a", which the page shows: compared where it is drawn, too.
    let procedure = "500 0 d0 q 500 0 0 700 0 0 cm /Gi Do Q";
    let with = |pixels: &[u8], word: &str| type3_page("/Resources << /XObject << /Gi 7 0 R >> >>", procedure, "", word, &[grey(4, 4, pixels, "")]);
    let base = with(&pixels, WORD);
    let inspection = run(&base, &with(&changed, WORD_BOX)).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    // Drawn by glyph "b", which no page shows yet: the font changed all the same.
    let font = String::from(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 500 700] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 6 0 R /b 7 0 R >> \
         /Encoding << /Type /Encoding /Differences [97 /a /b] >> /FirstChar 97 /LastChar 98 /Widths [500 500] /Resources << /XObject << /Gi 8 0 R >> >> >>",
    );
    let undrawn = |pixels: &[u8], word: &str| {
        page_file(
            "",
            "",
            &format!("BT /T3 20 Tf 30 150 Td (aa) Tj ET\n{word}"),
            "/Font << /T3 5 0 R >>",
            &[font.clone().into_bytes(), stream("", GLYPH.as_bytes()), stream("", procedure.as_bytes()), grey(4, 4, pixels, "")],
        )
    };
    let base = undrawn(&pixels, WORD);
    assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default());
    let inspection = run(&base, &undrawn(&changed, WORD_BOX)).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "a glyph no page shows: {inspection:?}");
}

// ── follow-up: a form drawn once is judged by its page ──────────────────────────────────────

#[test]
fn a_redaction_inside_a_form_drawn_once_is_judged_by_the_page() {
    // Page content wrapped in one page-wide form, as imposition and merging tools write it: the
    // redactor rewrites the form.
    let wrapped = |content: &str| page_file("", "", "q /Pg Do Q", "/XObject << /Pg 5 0 R >>", &[form("0 0 200 300", content)]);
    let base = wrapped(&format!("{}\n{WORD}", text(20, 150, "Alpha")));
    let redacted = wrapped(&format!("{}\n{WORD_BOX}", text(20, 150, "Alpha")));
    let inspection = run(&base, &redacted).unwrap();
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!(inspection.redaction_areas.len(), 1);
    // A change the page shows outside the box still counts (grey: a black box is a box).
    let changed = wrapped(&format!("{}\n{WORD_BOX}\n0.5 g 20 250 30 30 re f", text(20, 150, "Alpha")));
    assert_eq!(run(&base, &changed).unwrap().content.pages_changed_outside_redactions, 1);
    // The same form drawn twice keeps its key: the redaction changes both places.
    let twice = |content: &str| page_file("", "", "q /Pg Do Q q /Pg Do Q", "/XObject << /Pg 5 0 R >>", &[form("0 0 200 300", content)]);
    let base = twice(&format!("{}\n{WORD}", text(20, 150, "Alpha")));
    let inspection = run(&base, &twice(&format!("{}\n{WORD_BOX}", text(20, 150, "Alpha")))).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
}

// ── follow-up: the outline is a fact ────────────────────────────────────────────────────────

/// A three-page file with an outline (object 3): an entry per page (objects 10–12; the second
/// goes there by a GoTo action, the third has a child, object 13, going to page 3), each titled
/// from `titles`; page `i` is object `4 + i`, its content `7 + i`.
fn outlined(titles: [&str; 4]) -> Vec<u8> {
    let page = |i: usize| {
        format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>", 7 + i).into_bytes()
    };
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R /Outlines 3 0 R /PageMode /UseOutlines >>".to_vec(),
        b"<< /Type /Pages /Kids [4 0 R 5 0 R 6 0 R] /Count 3 /MediaBox [0 0 200 300] >>".to_vec(),
        b"<< /Type /Outlines /First 10 0 R /Last 12 0 R /Count 4 >>".to_vec(),
        page(0),
        page(1),
        page(2),
    ];
    for i in 0..3 {
        bodies.push(stream("", text(20, 150, &format!("Page {}", i + 1)).as_bytes()));
    }
    bodies.push(format!("<< /Title ({}) /Parent 3 0 R /Next 11 0 R /Dest [4 0 R /XYZ 0 300 0] >>", titles[0]).into_bytes());
    bodies.push(format!("<< /Title ({}) /Parent 3 0 R /Prev 10 0 R /Next 12 0 R /A << /S /GoTo /D [5 0 R /Fit] >> >>", titles[1]).into_bytes());
    bodies
        .push(format!("<< /Title ({}) /Parent 3 0 R /Prev 11 0 R /First 13 0 R /Last 13 0 R /Count 1 /Dest [6 0 R /Fit] >>", titles[2]).into_bytes());
    bodies.push(format!("<< /Title ({}) /Parent 12 0 R /Dest [6 0 R /XYZ 0 150 0] >>", titles[3]).into_bytes());
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

const TITLES: [&str; 4] = ["Parties", "Scope", "Term", "Renewal"];

fn session(base: &[u8], edits: Vec<crate::Edit>) -> Vec<u8> {
    let mut session = crate::Session::new().with_clock(|| 1_700_000_000);
    let id = session.open("base.pdf", None, std::sync::Arc::new(base.to_vec()), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

#[test]
fn page_edits_of_a_file_with_an_outline_are_reported_not_refused() {
    use crate::Edit;
    let base = outlined(TITLES);
    assert_eq!(run(&base, &base).unwrap().outlines, OutlineChanges::default());
    // A rotation leaves every destination where it was.
    let rotated = run(&base, &session(&base, vec![Edit::RotatePages { pages: vec![1], degrees: 90 }])).unwrap();
    assert_eq!((rotated.pages.rotated, rotated.outlines), (1, OutlineChanges::default()));
    // Page 2 deleted: its entry loses its destination, and the entries after it go to page 2.
    let deleted = run(&base, &session(&base, vec![Edit::DeletePages { pages: vec![1] }])).unwrap();
    assert_eq!(deleted.pages.deleted, 1);
    assert_eq!(deleted.outlines, OutlineChanges { destinations_changed: 3, ..OutlineChanges::default() }, "{deleted:?}");
    // Page 3 moved first: every destination's page number changes.
    let moved = run(&base, &session(&base, vec![Edit::MovePages { pages: vec![2], to: 0 }])).unwrap();
    assert!(moved.pages.moved >= 1, "{moved:?}");
    assert_eq!(moved.outlines, OutlineChanges { destinations_changed: 4, ..OutlineChanges::default() }, "{moved:?}");
    assert_eq!(moved.content, ContentChanges::default());
}

#[test]
fn outline_titles_entries_and_other_changes() {
    let base = outlined(TITLES);
    // A title changed in an appended update.
    let renamed = update_objects(&base, &[(13, b"<< /Title (Extension) /Parent 12 0 R /Dest [6 0 R /XYZ 0 150 0] >>".to_vec())], 14);
    assert_eq!(run(&base, &renamed).unwrap().outlines, OutlineChanges { titles_changed: 1, ..OutlineChanges::default() });
    // An entry added after the last, and one removed (the child).
    let added = update_objects(
        &base,
        &[
            (12, b"<< /Title (Term) /Parent 3 0 R /Prev 11 0 R /Next 14 0 R /First 13 0 R /Last 13 0 R /Count 1 /Dest [6 0 R /Fit] >>".to_vec()),
            (14, b"<< /Title (Signatures) /Parent 3 0 R /Prev 12 0 R /Dest [6 0 R /Fit] >>".to_vec()),
        ],
        15,
    );
    assert_eq!(run(&base, &added).unwrap().outlines, OutlineChanges { added: 1, ..OutlineChanges::default() });
    let removed = update_objects(&base, &[(12, b"<< /Title (Term) /Parent 3 0 R /Prev 11 0 R /Dest [6 0 R /Fit] >>".to_vec())], 14);
    assert_eq!(run(&base, &removed).unwrap().outlines, OutlineChanges { removed: 1, ..OutlineChanges::default() });
    // A destination moved to another page.
    let retargeted = update_objects(&base, &[(10, b"<< /Title (Parties) /Parent 3 0 R /Next 11 0 R /Dest [6 0 R /XYZ 0 300 0] >>".to_vec())], 14);
    assert_eq!(run(&base, &retargeted).unwrap().outlines, OutlineChanges { destinations_changed: 1, ..OutlineChanges::default() });
    // What no fact reports fails: a style changed, an action that is not a destination.
    let styled = update_objects(&base, &[(10, b"<< /Title (Parties) /Parent 3 0 R /Next 11 0 R /F 2 /Dest [4 0 R /XYZ 0 300 0] >>".to_vec())], 14);
    assert!(edited_error(run(&base, &styled)).contains("outline entry"));
    let scripted =
        update_objects(&base, &[(10, b"<< /Title (Parties) /Parent 3 0 R /Next 11 0 R /A << /S /JavaScript /JS (app.alert(1)) >> >>".to_vec())], 14);
    assert!(edited_error(run(&base, &scripted)).contains("outline entry"));
    let added_link = update_objects(
        &base,
        &[
            (12, b"<< /Title (Term) /Parent 3 0 R /Prev 11 0 R /Next 14 0 R /First 13 0 R /Last 13 0 R /Count 1 /Dest [6 0 R /Fit] >>".to_vec()),
            (14, b"<< /Title (Site) /Parent 3 0 R /Prev 12 0 R /A << /S /URI /URI (https://example.com) >> >>".to_vec()),
        ],
        15,
    );
    assert!(edited_error(run(&base, &added_link)).contains("added outline entry"));
}

// ── follow-up: what other viewers may show otherwise than hayro ─────────────────────────────

#[test]
fn text_that_only_clips_is_text() {
    // Rendering mode 7 draws nothing and adds the glyphs to the clip; other readers extract the
    // text all the same.
    let clipped = |word: &str| format!("BT 7 Tr /F1 12 Tf 20 200 Td ({word}) Tj ET");
    let base = page_file("", "", &format!("{}\n{WORD}", text(20, 150, "Alpha")), "", &[]);
    let added = page_file("", "", &format!("{}\n{WORD_BOX}\n{}", text(20, 150, "Alpha"), clipped("Secret")), "", &[]);
    let inspection = run(&base, &added).unwrap();
    assert_eq!(inspection.text.pages_with_text_added, 1, "{inspection:?}");
    // Removed where no box is: a change outside the redactions, though no pixel changed.
    let base = page_file("", "", &format!("{}\n{}\n{WORD}", text(20, 150, "Alpha"), clipped("Hidden")), "", &[]);
    let removed = page_file("", "", &format!("{}\n{WORD_BOX}", text(20, 150, "Alpha")), "", &[]);
    let inspection = run(&base, &removed).unwrap();
    assert_eq!(inspection.text.pages_with_text_removed, 1, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
    assert_eq!(run(&base, &base).unwrap().text, TextChanges::default());
}

#[test]
fn a_redaction_on_a_page_drawn_with_what_viewers_render_differently_is_outside_the_redactions() {
    let redacted = |state: &str, objects: &[Vec<u8>], word: &str| {
        page_file(
            "",
            "",
            &format!("q /GS1 gs 0.5 g 20 200 50 50 re f Q\n{}\n{word}", text(20, 150, "Alpha")),
            &format!("/ExtGState << /GS1 {state} >>"),
            objects,
        )
    };
    let function = b"<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >>".to_vec();
    for (case, state, objects) in [
        ("a blend mode", "<< /BM /Multiply >>", vec![]),
        ("a blend mode in an array", "<< /BM [/Screen /Normal] >>", vec![]),
        ("a transfer function", "<< /TR 5 0 R >>", vec![function.clone()]),
        ("a transfer function, TR2", "<< /TR2 5 0 R >>", vec![function.clone()]),
    ] {
        let base = redacted(state, &objects, WORD);
        assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default(), "{case}");
        let inspection = run(&base, &redacted(state, &objects, WORD_BOX)).unwrap();
        assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{case}: {inspection:?}");
    }
    // What every viewer draws alike is no risk: the same redaction is accepted.
    for (case, state) in [("/Normal", "<< /BM /Normal >>"), ("/Compatible", "<< /BM /Compatible >>"), ("identity transfer", "<< /TR /Identity >>")] {
        let base = redacted(state, &[], WORD);
        let inspection = run(&base, &redacted(state, &[], WORD_BOX)).unwrap();
        assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{case}: {inspection:?}");
        assert_eq!(inspection.redaction_areas.len(), 1, "{case}");
    }
    // A knockout group, drawn by a form.
    let knockout = |word: &str| {
        page_file(
            "",
            "",
            &format!("/Fm Do\n{}\n{word}", text(20, 150, "Alpha")),
            "/XObject << /Fm 5 0 R >>",
            &[stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /K true >>", b"0.5 g 20 200 50 50 re f")],
        )
    };
    let inspection = run(&knockout(WORD), &knockout(WORD_BOX)).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "a knockout group: {inspection:?}");
}

#[test]
fn annotation_flags_states_and_layers_are_reported() {
    let annotated = |extra: &str| {
        page_file(
            "/OCProperties << /OCGs [7 0 R] /D << >> >>",
            "/Annots [5 0 R]",
            &text(20, 150, "Alpha"),
            "",
            &[
                format!("<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /AP << /N 6 0 R {extra} >> >>").into_bytes(),
                appearance_stream("", SQUARE.as_bytes()),
                b"<< /Type /OCG /Name (Layer) >>".to_vec(),
            ],
        )
    };
    let base = annotated("");
    let square = |entries: &str| format!("<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /AP << /N 6 0 R >> {entries} >>").into_bytes();
    for (case, entries) in [("hidden", "/F 2"), ("not viewed", "/F 32"), ("not printed", "/F 0"), ("in a layer", "/OC 7 0 R")] {
        let edited = update_objects(&base, &[(5, square(entries))], 8);
        assert_eq!(run(&base, &edited).unwrap().annotations.changed, 1, "{case}");
    }
    for (case, states) in [("a down appearance", "/D 6 0 R"), ("a rollover appearance", "/R 6 0 R")] {
        let edited = update_objects(
            &base,
            &[(5, format!("<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /AP << /N 6 0 R {states} >> >>").into_bytes())],
            8,
        );
        assert_eq!(run(&base, &edited).unwrap().annotations.changed, 1, "{case}");
    }
    // In a rewrite, the annotation with an appearance state added is another annotation.
    let inspection = run(&base, &annotated("/D 6 0 R")).unwrap();
    assert_eq!((inspection.annotations.removed, inspection.annotations.drawings), (1, 1), "{inspection:?}");
}
