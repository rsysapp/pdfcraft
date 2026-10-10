#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::tests::fixture;
use crate::{Edit, Markup, NewAnnotation, Session, Shape, Style, rect_quad};

// ── building files by hand ──────────────────────────────────────────────────────────────────

/// A stream object body: `<< dict /Length n >> stream … endstream`.
fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

fn flate_stream(dict: &str, data: &[u8]) -> Vec<u8> {
    stream(&format!("{dict} /Filter /FlateDecode"), &pdfcraft_cos::Stream::flate(pdfcraft_cos::Dict::new(), data).raw)
}

/// A classic-xref file: object `i + 1` is `bodies[i]`; the catalog is object 1.
fn pdf(bodies: &[Vec<u8>]) -> Vec<u8> {
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

fn startxref(bytes: &[u8]) -> usize {
    let at = bytes.windows(9).rposition(|w| w == b"startxref").unwrap();
    std::str::from_utf8(&bytes[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap()
}

/// Append an update holding `objects` to `base`. `trailer` gets the new section's offset and the
/// base's `startxref` and returns the trailer's entries (without `<< >>`).
fn update_with(base: &[u8], objects: &[(u32, Vec<u8>)], trailer: impl Fn(usize, usize) -> String) -> Vec<u8> {
    let prev = startxref(base);
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
    out.extend_from_slice(format!("trailer\n<< {} >>\nstartxref\n{xref}\n%%EOF\n", trailer(xref, prev)).as_bytes());
    out
}

/// Append an update the usual way: `/Size`, `/Root 1 0 R`, `/Prev` the base's section.
fn update(base: &[u8], objects: &[(u32, Vec<u8>)], size: u32) -> Vec<u8> {
    update_with(base, objects, |_, prev| format!("/Size {size} /Root 1 0 R /Prev {prev}"))
}

/// A page of a hand-built file: its content and extra page-dictionary entries.
struct PageSpec {
    content: Vec<u8>,
    extra: String,
}

fn page(content: &str) -> PageSpec {
    PageSpec { content: content.as_bytes().to_vec(), extra: String::new() }
}

/// A file of 200 × 300 pages: object 1 the catalog, 2 the page tree, 3 Helvetica (`/F1`), then
/// `extras` (objects 4…), then each page and its content (see [`page_object`]). Every page's
/// resources name `/F1` and the images `/Im1`… for the extras that are images.
fn pages_pdf(pages: &[PageSpec], extras: &[Vec<u8>]) -> Vec<u8> {
    let first_page = 4 + extras.len();
    let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", first_page + 2 * i)).collect();
    let images: String =
        (0..extras.len()).filter(|i| extras[*i].starts_with(b"<< /Type /XObject")).map(|i| format!("/Im{} {} 0 R ", i + 1, i + 4)).collect();
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {} /MediaBox [0 0 200 300] >>", kids.join(" "), pages.len()).into_bytes(),
        format!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding /FirstChar 32 /LastChar 126 /Widths [{HELVETICA_WIDTHS}] >>")
            .into_bytes(),
    ];
    bodies.extend(extras.iter().cloned());
    for (i, spec) in pages.iter().enumerate() {
        let content = first_page + 2 * i + 1;
        bodies.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /Contents {content} 0 R /Resources << /Font << /F1 3 0 R >> /XObject << {images}>> /ExtGState << /Half << /ca 0.5 >> >> >> {} >>",
                spec.extra
            )
            .into_bytes(),
        );
        bodies.push(stream("", &spec.content));
    }
    pdf(&bodies)
}

/// Helvetica's advance widths for codes 32–126 (WinAnsi), as producers write them: the redactor
/// keeps the rest of a line in place with the font's `/Widths`.
const HELVETICA_WIDTHS: &str = "278 278 355 556 556 889 667 191 333 333 389 584 278 333 278 278 556 556 556 556 556 556 556 556 556 556 \
    278 278 584 584 584 556 1015 667 667 722 722 667 611 778 722 278 500 667 556 833 722 778 667 778 722 667 611 722 667 944 667 667 611 \
    278 278 278 469 556 333 556 556 500 556 556 278 556 556 222 222 500 222 833 556 556 556 556 333 500 278 556 500 722 500 500 500 334 260 334 584";

/// The width of `text` in 12 pt Helvetica (the test font), in points.
fn width_of(text: &str) -> f32 {
    let widths: Vec<f32> = HELVETICA_WIDTHS.split_whitespace().map(|w| w.parse().unwrap()).collect();
    text.bytes().map(|b| widths[usize::from(b) - 32]).sum::<f32>() * 12.0 / 1000.0
}

/// The object number of page `index` in [`pages_pdf`] with `extras` extra objects.
fn page_object(extras: usize, index: usize) -> u32 {
    (4 + extras + 2 * index) as u32
}

/// An 8 × 8 grey image XObject: a texture around `shade`, as a scan has.
fn image(shade: u8) -> Vec<u8> {
    let pixels: Vec<u8> = (0..64u8).map(|i| shade.wrapping_add(i.wrapping_mul(37) % 48)).collect();
    stream("/Type /XObject /Subtype /Image /Width 8 /Height 8 /ColorSpace /DeviceGray /BitsPerComponent 8", &pixels)
}

/// A text-less scanned page: image `/Im<n>` drawn over the whole page.
fn scan_page(n: usize) -> PageSpec {
    page(&format!("q 200 0 0 300 0 0 cm /Im{n} Do Q"))
}

fn text_line(x: f32, y: f32, text: &str) -> String {
    format!("BT /F1 12 Tf {x} {y} Td ({text}) Tj ET\n")
}

/// A small grey PNG, for an image placed on a page.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&vec![128u8; (width * height) as usize]).unwrap();
    }
    bytes
}

fn session_edit(base: Vec<u8>, edits: Vec<Edit>) -> Vec<u8> {
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    let id = session.open("base.pdf", None, Arc::new(base), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

fn edited(pages: usize, edits: Vec<Edit>) -> (Vec<u8>, Vec<u8>) {
    let base = fixture(pages);
    let saved = session_edit(base.clone(), edits);
    (base, saved)
}

fn annotation(page: usize, shape: Shape) -> Edit {
    Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: String::new(), author: "Sari".into() })
}

fn redact(rect: [f64; 4]) -> Shape {
    Shape::Redact { quads: vec![rect_quad(rect)], overlay: String::new(), look: Default::default() }
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

// ── the original tests, on the new shape ────────────────────────────────────────────────────

#[test]
fn finds_nothing_in_an_unchanged_file() {
    let base = fixture(2);
    let inspection = run(&base, &base).unwrap();
    assert_eq!(
        inspection,
        Inspection { base_kept: true, pages: PageChanges { before: 2, after: 2, ..PageChanges::default() }, ..Inspection::default() }
    );
}

#[test]
fn reports_a_rotation_as_an_appended_update() {
    let (base, saved) = edited(2, vec![Edit::RotatePages { pages: vec![0], degrees: 90 }]);
    let inspection = run(&base, &saved).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.pages, PageChanges { before: 2, after: 2, rotated: 1, ..PageChanges::default() });
    assert_eq!(inspection.content, ContentChanges::default());
}

#[test]
fn reports_deleted_moved_and_inserted_pages() {
    let (base, saved) = edited(3, vec![Edit::DeletePages { pages: vec![2] }]);
    assert_eq!(run(&base, &saved).unwrap().pages, PageChanges { before: 3, after: 2, deleted: 1, ..PageChanges::default() });
    let (base, saved) = edited(3, vec![Edit::MovePages { pages: vec![2], to: 0 }]);
    let inspection = run(&base, &saved).unwrap();
    assert_eq!(inspection.pages.moved, 1);
    assert_eq!(inspection.content, ContentChanges::default());
    let (base, saved) = edited(2, vec![Edit::InsertBlankPage { at: 2, width: 200.0, height: 300.0 }]);
    assert_eq!(run(&base, &saved).unwrap().pages, PageChanges { before: 2, after: 3, inserted: 1, ..PageChanges::default() });
}

#[test]
fn reports_an_applied_redaction_as_a_rewrite_that_removed_text() {
    let (base, saved) = edited(2, vec![annotation(0, redact([80.0, 140.0, 100.0, 180.0])), Edit::ApplyRedactions { pages: None }]);
    let inspection = run(&base, &saved).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 });
    assert_eq!(inspection.content, ContentChanges { pages_with_content_removed: 1, ..ContentChanges::default() });
    assert_eq!(inspection.annotations, AnnotationChanges::default());
    assert_eq!(inspection.pages, PageChanges { before: 2, after: 2, ..PageChanges::default() });
}

// A signature or a stamp image is a typical redaction target (D11): a rewrite that removed
// only an image is a redaction too. The fixture's text sits at y 126–150 of a 200 × 300 page
// (display space, as `AddImage` and the marks take it); the image sits well below it.
#[test]
fn reports_a_redaction_that_removed_only_an_image() {
    let (_, with_image) =
        edited(1, vec![Edit::AddImage { page: 0, rect: Some([20.0, 220.0, 60.0, 260.0]), name: "seal.png".into(), bytes: Arc::new(png(8, 8)) }]);
    let redacted = session_edit(with_image.clone(), vec![annotation(0, redact([10.0, 210.0, 70.0, 270.0])), Edit::ApplyRedactions { pages: None }]);
    let inspection = run(&with_image, &redacted).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.text, TextChanges::default());
    assert_eq!(inspection.content, ContentChanges { pages_with_images_removed: 1, pages_with_content_removed: 1, ..ContentChanges::default() });
}

#[test]
fn counts_a_redaction_mark_saved_without_applying_it() {
    let (base, saved) = edited(2, vec![annotation(0, redact([80.0, 140.0, 100.0, 180.0]))]);
    let inspection = run(&base, &saved).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.annotations, AnnotationChanges { redaction_marks: 1, ..AnnotationChanges::default() });
    assert_eq!(inspection.text, TextChanges::default());
    assert_eq!(inspection.content, ContentChanges::default());
}

#[test]
fn counts_drawings_and_comments_added() {
    let ink = Shape::Ink { strokes: vec![vec![[10.0, 10.0], [60.0, 60.0]]] };
    let highlight = Shape::TextMarkup { kind: Markup::Highlight, quads: vec![rect_quad([20.0, 140.0, 100.0, 180.0])] };
    let (base, saved) = edited(1, vec![annotation(0, ink), annotation(0, highlight)]);
    let annotations = run(&base, &saved).unwrap().annotations;
    assert_eq!(annotations, AnnotationChanges { drawings: 1, comments: 1, ..AnnotationChanges::default() });
}

fn encrypted(user: &str) -> Vec<u8> {
    let mut document = pdfcraft_cos::Document::open(Arc::new(fixture(1))).unwrap();
    document
        .set_encryption(&pdfcraft_cos::NewEncryption {
            algorithm: pdfcraft_cos::Algorithm::Aes256,
            user_password: user,
            owner_password: "owner",
            permissions: -1,
            encrypt_metadata: true,
            seed: [7; 32],
        })
        .unwrap();
    pdfcraft_cos::write_full(&document, &Default::default()).unwrap()
}

#[test]
fn refuses_encrypted_output_whether_or_not_it_opens() {
    let base = fixture(1);
    assert_eq!(run(&base, &encrypted("")), Err(InspectError::Encrypted));
    assert_eq!(run(&base, &encrypted("pw")), Err(InspectError::Encrypted));
}

#[test]
fn reports_a_form_field_filled_in() {
    let base = crate::tests::form_fixture();
    let saved = session_edit(base.clone(), vec![Edit::SetFieldValue { name: "name".into(), value: crate::FieldValue::Text("Ada".into()) }]);
    let inspection = run(&base, &saved).unwrap();
    assert_eq!(inspection.form_fields_changed, 1);
    // The field's widget is the form, not an annotation someone added.
    assert_eq!(inspection.annotations, AnnotationChanges::default());
}

#[test]
fn reports_a_rewrite_whose_base_is_not_a_prefix() {
    let base = fixture(2);
    let mut rewritten = fixture(2);
    // Same pages, different bytes up front: a full rewrite, not an appended update. The comment
    // shifts every object, so the cross-reference table is rewritten to match.
    rewritten.splice(9..9, b"% rewritten\n".iter().copied());
    let rewritten = pdfcraft_cos::write_full(&pdfcraft_cos::Document::open(Arc::new(rewritten)).unwrap(), &Default::default()).unwrap();
    let inspection = run(&base, &rewritten).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.pages, PageChanges { before: 2, after: 2, ..PageChanges::default() });
    assert_eq!(inspection.content, ContentChanges::default());
    assert_eq!(inspection.text, TextChanges::default());
}

#[test]
fn refuses_more_pages_than_the_bound_before_reading_them() {
    let small = fixture(1);
    let large = fixture(MAX_PAGES + 1);
    let started = Instant::now();
    assert_eq!(run(&large, &small), Err(InspectError::TooComplex));
    assert_eq!(run(&small, &large), Err(InspectError::TooComplex));
    // Reading 2,001 pages' text would take far longer: the bound is checked from the page tree.
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn never_panics_on_garbage_or_a_cut_file() {
    let base = fixture(2);
    // A fixed pseudo-random stream, so the case is the same on every run.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let noise: Vec<u8> = (0..4096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect();
    let mut header_and_noise = b"%PDF-1.7\n".to_vec();
    header_and_noise.extend_from_slice(&noise);
    for bad in [Vec::new(), noise.clone(), header_and_noise, b"%PDF-1.7\n".to_vec()] {
        assert!(matches!(run(&bad, &base), Err(InspectError::Base(_))));
        assert!(matches!(run(&base, &bad), Err(InspectError::Edited(_))));
    }
    for length in [base.len() / 4, base.len() / 2, base.len().saturating_sub(10)] {
        let cut = base.get(..length).unwrap().to_vec();
        assert!(!matches!(run(&base, &cut), Err(InspectError::Edited(m)) if m.starts_with("internal error")));
        assert!(!matches!(run(&cut, &base), Err(InspectError::Base(m)) if m.starts_with("internal error")));
    }
}

#[test]
fn names_the_file_it_cannot_read() {
    let base = fixture(1);
    assert!(matches!(run(b"not a pdf", &base), Err(InspectError::Base(_))));
    assert!(matches!(run(&base, b"PK\x03\x04 not a pdf"), Err(InspectError::Edited(_))));
}

// ── 1. one structure source: the reviewer's parser split ────────────────────────────────────

/// The base page's content replaced and a Redact mark added, in an appended update whose trailer
/// is built by `trailer` (xref offset, base startxref).
fn split_update(trailer: impl Fn(usize, usize) -> String) -> (Vec<u8>, Vec<u8>) {
    let base = pages_pdf(&[page(&text_line(20.0, 150.0, "Alpha Bravo"))], &[]);
    let page_num = page_object(0, 0);
    let objects = vec![
        (page_num, b"<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources << /Font << /F1 3 0 R >> >> /Annots [8 0 R] >>".to_vec()),
        (7, stream("", text_line(20.0, 150.0, "Zulu Yankee").as_bytes())),
        (8, format!("<< /Type /Annot /Subtype /Redact /Rect [20 140 80 160] /P {page_num} 0 R >>").into_bytes()),
    ];
    let saved = update_with(&base, &objects, trailer);
    (base, saved)
}

#[test]
fn refuses_an_update_whose_prev_points_at_itself() {
    let (base, saved) = split_update(|xref, _| format!("/Size 9 /Root 1 0 R /Prev {xref}"));
    edited_error(run(&base, &saved));
}

#[test]
fn refuses_an_update_whose_root_is_not_a_reference() {
    let (base, saved) = split_update(|_, prev| format!("/Size 9 /Root (x) /Prev {prev}"));
    edited_error(run(&base, &saved));
}

#[test]
fn the_same_update_with_a_sound_trailer_reports_the_mark_and_the_text() {
    let (base, saved) = split_update(|_, prev| format!("/Size 9 /Root 1 0 R /Prev {prev}"));
    let inspection = run(&base, &saved).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.annotations.redaction_marks, 1);
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 1 });
    assert_eq!(inspection.content.pages_with_content_changed, 1);
}

#[test]
fn refuses_an_update_whose_prev_points_forwards() {
    // Two appended sections, the newer written first: its /Prev points to the older one after it.
    let base = fixture(1);
    let base_prev = startxref(&base);
    let mut out = base.clone();
    let annot_at = out.len();
    out.extend_from_slice(b"7 0 obj\n<< /Type /Annot /Subtype /Redact /Rect [20 140 80 160] >>\nendobj\n");
    let newer = out.len();
    let newer_section = |older: usize| {
        format!("xref\n0 1\n0000000000 65535 f \n7 1\n{annot_at:010} 00000 n \ntrailer\n<< /Size 8 /Root 1 0 R /Prev {older:010} >>\n")
    };
    let length = newer_section(0).len();
    let older = newer + length;
    out.extend_from_slice(newer_section(older).as_bytes());
    out.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 8 /Root 1 0 R /Prev {base_prev} >>\n").as_bytes());
    out.extend_from_slice(format!("startxref\n{newer}\n%%EOF\n").as_bytes());
    edited_error(run(&base, &out));
}

// ── 2. text that cannot be read is an error, a page without text is not ────────────────────

#[test]
fn refuses_a_page_whose_glyphs_have_no_text() {
    let base = pages_pdf(&[page(&text_line(20.0, 150.0, "Alpha"))], &[]);
    // A CID font with Identity encoding, no font program and no ToUnicode: glyphs are drawn
    // (by a substitute) but nothing says which characters they are.
    let cid = b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Unknown /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor << /Type /FontDescriptor /FontName /Unknown /Flags 4 /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 /CapHeight 700 /StemV 80 >> >>".to_vec();
    let type0 = b"<< /Type /Font /Subtype /Type0 /BaseFont /Unknown /Encoding /Identity-H /DescendantFonts [4 0 R] >>".to_vec();
    let edited = {
        let mut bodies = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [6 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            cid,
            type0,
        ];
        bodies.push(b"<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources << /Font << /F1 3 0 R /F2 5 0 R >> >> >>".to_vec());
        bodies.push(stream("", b"BT /F2 12 Tf 20 150 Td <0024004F0053004B0044> Tj ET"));
        pdf(&bodies)
    };
    let message = edited_error(run(&base, &edited));
    assert!(message.contains("text"), "{message}");
    // The same page as the base is refused too: its facts can't be established.
    assert!(matches!(run(&edited, &edited), Err(InspectError::Base(_))));
}

#[test]
fn a_page_drawn_without_text_reports_nothing() {
    // Outlines (paths) only: no glyphs, so no text, and nothing failed.
    let base = pages_pdf(&[page("0 0 1 rg 20 140 m 40 160 l 60 140 l h f\n20 100 40 20 re f")], &[]);
    let inspection = run(&base, &base).unwrap();
    assert_eq!(
        inspection,
        Inspection { base_kept: true, pages: PageChanges { before: 1, after: 1, ..PageChanges::default() }, ..Inspection::default() }
    );
}

// ── 3–5. rewrites are checked visually; scans pair by their images ──────────────────────────

#[test]
fn a_rewrite_that_removes_text_without_a_box_changes_pages_outside_redactions() {
    let base = pages_pdf(&[page(&text_line(20.0, 150.0, "Alpha Bravo Charlie")), page(&text_line(20.0, 150.0, "Delta"))], &[]);
    // "Bravo" left out of page 1 (Charlie stays where it was), page 2 emptied; no box anywhere.
    let edited = pages_pdf(&[page(&format!("{}{}", text_line(20.0, 150.0, "Alpha"), text_line(88.0, 150.0, "Charlie"))), page("")], &[]);
    let inspection = run(&base, &edited).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 2, pages_with_text_added: 0 });
    assert_eq!(inspection.content.pages_changed_outside_redactions, 2);
}

#[test]
fn deleting_a_page_of_a_scan_in_a_rewrite_is_a_deleted_page() {
    let base = pages_pdf(&[scan_page(1), scan_page(2), scan_page(3)], &[image(40), image(120), image(200)]);
    let edited = pages_pdf(&[scan_page(1), scan_page(3)], &[image(40), image(120), image(200)]);
    let inspection = run(&base, &edited).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.pages, PageChanges { before: 3, after: 2, deleted: 1, ..PageChanges::default() });
    assert_eq!(inspection.content, ContentChanges::default());
}

#[test]
fn replacing_a_scan_with_another_image_is_a_change_outside_redactions() {
    let base = pages_pdf(&[scan_page(1)], &[image(40), image(200)]);
    let edited = pages_pdf(&[scan_page(2)], &[image(40), image(200)]);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.pages, PageChanges { before: 1, after: 1, ..PageChanges::default() });
    assert_eq!(
        inspection.content,
        ContentChanges {
            pages_with_images_removed: 1,
            pages_with_images_added: 1,
            pages_with_content_removed: 1,
            pages_changed_outside_redactions: 1,
            ..ContentChanges::default()
        }
    );
}

#[test]
fn images_inside_form_xobjects_and_inline_images_are_seen() {
    let form = stream("/Type /XObject /Subtype /Form /BBox [0 0 1 1] /Resources << /XObject << /Im 4 0 R >> >>", b"/Im Do");
    let base = pages_pdf(
        &[page("q 100 0 0 100 0 0 cm /Im2 Do Q q 50 0 0 50 100 100 cm BI /W 2 /H 2 /CS /G /BPC 8 ID \x10\x20\x30\x40 EI Q")],
        &[image(40), form],
    );
    // The form draws another image now, and the inline image is gone.
    let form =
        String::from_utf8(stream("/Type /XObject /Subtype /Form /BBox [0 0 1 1] /Resources << /XObject << /Im 7 0 R >> >>", b"/Im Do")).unwrap();
    let form = form.replace("/Im 7 0 R", "/Im 8 0 R").into_bytes();
    let edited = update(&base, &[(5, form), (8, image(90)), (page_object(2, 0) + 1, stream("", b"q 100 0 0 100 0 0 cm /Im2 Do Q"))], 9);
    let inspection = run(&base, &edited).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.content.pages_with_images_removed, 1);
    assert_eq!(inspection.content.pages_with_images_added, 1);
    assert_eq!(inspection.content.pages_with_content_changed, 1);
}

// ── 6. appended updates prove page content unchanged ────────────────────────────────────────

fn content_update(base: &[u8], content: &str, resources: &str, extra: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let page_num = page_object(0, 0);
    let mut objects = vec![
        (page_num, format!("<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources << /Font << /F1 3 0 R >> {resources} >> >>").into_bytes()),
        (7, stream("", content.as_bytes())),
    ];
    objects.extend(extra.iter().cloned());
    update(base, &objects, 9)
}

#[test]
fn appended_content_changes_are_reported() {
    let original = text_line(20.0, 150.0, "Alpha Bravo");
    let base = pages_pdf(&[page(&original)], &[]);
    let cases = [
        ("reordered words", content_update(&base, &text_line(20.0, 150.0, "Bravo Alpha"), "", &[])),
        ("white box over text", content_update(&base, &format!("{original}1 g 18 145 80 20 re f\n"), "", &[])),
        ("invisible text", content_update(&base, &format!("{original}BT 3 Tr /F1 12 Tf 20 100 Td (Hidden) Tj ET\n"), "", &[])),
        (
            "image over text",
            content_update(&base, &format!("{original}q 80 0 0 20 18 145 cm /Im8 Do Q\n"), "/XObject << /Im8 8 0 R >>", &[(8, image(255))]),
        ),
    ];
    for (case, saved) in cases {
        let inspection = run(&base, &saved).unwrap();
        assert!(inspection.base_kept, "{case}");
        if case == "reordered words" {
            // The same characters in another order: text removed and text added.
            assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 1 });
        }
        assert_eq!(inspection.content.pages_with_content_changed, 1, "{case}: {inspection:?}");
        assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{case}");
    }
}

#[test]
fn a_changed_page_key_counts_as_changed_content() {
    let base = pages_pdf(&[page(&text_line(20.0, 150.0, "Alpha"))], &[]);
    let page_num = page_object(0, 0);
    let saved = update(
        &base,
        &[(page_num, b"<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> /CropBox [0 0 100 100] >>".to_vec())],
        6,
    );
    assert_eq!(run(&base, &saved).unwrap().content.pages_with_content_changed, 1);
}

// ── 7. annotations by identity ──────────────────────────────────────────────────────────────

fn annotated_base() -> Vec<u8> {
    let square = b"<< /Type /Annot /Subtype /Square /Rect [20 20 80 80] /C [1 0 0] /F 4 >>".to_vec();
    let highlight =
        b"<< /Type /Annot /Subtype /Highlight /Rect [20 140 80 160] /QuadPoints [20 160 80 160 20 140 80 140] /C [1 1 0] /F 4 >>".to_vec();
    let mut first = page(&text_line(20.0, 150.0, "Alpha"));
    first.extra = "/Annots [4 0 R 5 0 R]".into();
    pages_pdf(&[first, page(&text_line(20.0, 150.0, "Bravo"))], &[square, highlight])
}

/// Objects of an appended update: (number, body).
type Objects = Vec<(u32, Vec<u8>)>;

fn page_dict(index: usize, annots: &str) -> Vec<u8> {
    format!(
        "<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> /XObject << >> >> {annots} >>",
        page_object(2, index) + 1
    )
    .into_bytes()
}

#[test]
fn annotations_are_compared_by_identity() {
    let base = annotated_base();
    let (first, second) = (page_object(2, 0), page_object(2, 1));
    let cases: Vec<(&str, Objects)> = vec![
        ("moved to another page", vec![(first, page_dict(0, "/Annots [5 0 R]")), (second, page_dict(1, "/Annots [4 0 R]"))]),
        ("hidden", vec![(4, b"<< /Type /Annot /Subtype /Square /Rect [20 20 80 80] /C [1 0 0] /F 2 >>".to_vec())]),
        ("colour", vec![(4, b"<< /Type /Annot /Subtype /Square /Rect [20 20 80 80] /C [1 1 1] /F 4 >>".to_vec())]),
        (
            "quad points",
            vec![(
                5,
                b"<< /Type /Annot /Subtype /Highlight /Rect [20 140 80 160] /QuadPoints [20 160 60 160 20 140 60 140] /C [1 1 0] /F 4 >>".to_vec(),
            )],
        ),
        ("nudged", vec![(4, b"<< /Type /Annot /Subtype /Square /Rect [20.4 20 80.4 80] /C [1 0 0] /F 4 >>".to_vec())]),
    ];
    for (case, objects) in cases {
        let inspection = run(&base, &update(&base, &objects, 10)).unwrap();
        assert_eq!(inspection.annotations, AnnotationChanges { changed: 1, ..AnnotationChanges::default() }, "{case}");
    }
    let deleted = update(&base, &[(first, page_dict(0, "/Annots [5 0 R]"))], 10);
    assert_eq!(run(&base, &deleted).unwrap().annotations, AnnotationChanges { removed: 1, ..AnnotationChanges::default() });
}

// ── 8–9. redaction marks and applied redactions ─────────────────────────────────────────────

#[test]
fn a_base_redaction_mark_is_not_counted_again() {
    let (_, marked) = edited(2, vec![annotation(0, redact([80.0, 140.0, 100.0, 180.0]))]);
    let rotated = session_edit(marked.clone(), vec![Edit::RotatePages { pages: vec![1], degrees: 90 }]);
    let inspection = run(&marked, &rotated).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.annotations, AnnotationChanges::default());
    assert_eq!(inspection.pages.rotated, 1);
}

#[test]
fn applying_a_base_redaction_mark_counts_it_as_applied() {
    // The fixture's "Page 1" (24 pt, baseline 150) spans y 145.2–169.2 in user space, as marks take it.
    let (_, marked) = edited(1, vec![annotation(0, redact([18.0, 140.0, 110.0, 175.0]))]);
    let applied = session_edit(marked.clone(), vec![Edit::ApplyRedactions { pages: None }]);
    let inspection = run(&marked, &applied).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.annotations, AnnotationChanges { redactions_applied: 1, ..AnnotationChanges::default() });
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0);
}

#[test]
fn a_real_redaction_through_a_word_adds_no_text() {
    // "Buyer pays Seller" in 12 pt Helvetica from x 20, baseline 150: "Buyer p" is 41.35 pt wide
    // and the line's glyphs span y 147.6–159.6 (user space, as marks take it).
    let base = pages_pdf(&[page(&text_line(20.0, 150.0, "Buyer pays Seller"))], &[]);
    let saved = session_edit(base.clone(), vec![annotation(0, redact([18.0, 144.0, 61.0, 163.0])), Edit::ApplyRedactions { pages: None }]);
    let inspection = run(&base, &saved).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 });
    assert_eq!(inspection.content, ContentChanges { pages_with_content_removed: 1, ..ContentChanges::default() });
    assert_eq!(inspection.annotations, AnnotationChanges::default());
    assert_eq!(inspection.pages, PageChanges { before: 1, after: 1, ..PageChanges::default() });
    // The applied box, where the mark was.
    assert_eq!(inspection.redaction_areas.len(), 1, "{:?}", inspection.redaction_areas);
    let (page, area) = inspection.redaction_areas[0];
    assert_eq!(page, 1);
    for (got, want) in area.iter().zip([18.0, 144.0, 61.0, 163.0]) {
        assert!((got - want).abs() < 0.5, "{area:?}");
    }
}

#[test]
fn a_redaction_across_two_lines_is_accepted() {
    // Lines 18 pt apart, so that neither line's glyphs (font box, as the redactor tests them)
    // reach the other line's box.
    let base = pages_pdf(&[page(&format!("{}{}", text_line(20.0, 150.0, "Buyer pays Seller"), text_line(20.0, 132.0, "the amount due")))], &[]);
    // One mark, a box per line ("Buyer p", "the a"), touching: together an L. (A box reaching a
    // neighbouring line's glyphs, or ending inside a glyph, removes glyphs whose visible parts lie
    // outside the boxes: a change outside the redaction.)
    let mark = Shape::Redact {
        quads: vec![rect_quad([18.0, 147.7, 61.0, 159.7]), rect_quad([18.0, 129.5, 46.5, 147.7])],
        overlay: String::new(),
        look: Default::default(),
    };
    let saved = session_edit(base.clone(), vec![annotation(0, mark), Edit::ApplyRedactions { pages: None }]);
    let inspection = run(&base, &saved).unwrap();
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 });
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0);
}

/// "Alpha Bravo Charlie" on one line, with each word placed where the whole line puts it.
fn three_words(bravo: bool, extra: &str) -> PageSpec {
    let bravo = if bravo { text_line(54.02, 150.0, "Bravo") } else { String::new() };
    page(&format!("{extra}{}{bravo}{}", text_line(20.0, 150.0, "Alpha"), text_line(88.7, 150.0, "Charlie")))
}

#[test]
fn the_visual_check_tells_boxes_from_other_changes() {
    let base = pages_pdf(&[three_words(true, "")], &[]);
    let outside = |edited: PageSpec| run(&base, &pages_pdf(&[edited], &[])).unwrap().content.pages_changed_outside_redactions;
    // A box drawn where "Bravo" was: a redaction.
    assert_eq!(outside(three_words(false, "0 g 53 146 33.4 15 re f\n")), 0);
    // The same word deleted under a white box, or with the page tinted so that the deletion
    // sits inside one big "fill": not a box.
    assert_eq!(outside(three_words(false, "1 g 53 146 33.4 15 re f 0 g\n")), 1);
    assert_eq!(outside(three_words(false, "0.94 g 0 0 200 300 re f 0 g\n")), 1);
    // A thin line added (a minus turned into a plus): not a box either.
    assert_eq!(outside(three_words(true, "0 g 60 140 0.5 20 re f\n")), 1);
    // A tinted page whose only remaining content is a solid square: the "fill" (the whole page)
    // has a hole where the square still shows.
    let square = "0 g 120 40 30 30 re f\n";
    let base = pages_pdf(&[page(&format!("{square}{}", text_line(20.0, 150.0, "Bravo")))], &[]);
    let tinted = pages_pdf(&[page(&format!("0.94 g 0 0 200 300 re f\n{square}"))], &[]);
    assert_eq!(run(&base, &tinted).unwrap().content.pages_changed_outside_redactions, 1);
}

#[test]
fn a_page_rotated_and_redacted_in_one_save_is_compared_upright() {
    let base = pages_pdf(&[page(&text_line(20.0, 150.0, "Buyer pays Seller"))], &[]);
    let saved = session_edit(
        base.clone(),
        vec![
            annotation(0, redact([18.0, 144.0, 61.0, 163.0])),
            Edit::ApplyRedactions { pages: None },
            Edit::RotatePages { pages: vec![0], degrees: 90 },
        ],
    );
    let inspection = run(&base, &saved).unwrap();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.pages.rotated, 1);
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0);
}

/// "Buyer may not terminate", or the same line with "not" left out and `cover` drawn over the gap
/// ("terminate" where it was).
fn may_not_terminate(cover: Option<&str>) -> Vec<u8> {
    let not_at = 20.0 + width_of("Buyer may ");
    let terminate_at = 20.0 + width_of("Buyer may not ");
    let content = match cover {
        None => text_line(20.0, 150.0, "Buyer may not terminate"),
        Some(paint) => {
            let box_ = format!("{paint} {} 146 {} 16 re f 0 g\n", not_at - 0.5, width_of("not") + 1.0);
            format!("{}{}{box_}", text_line(20.0, 150.0, "Buyer may"), text_line(terminate_at, 150.0, "terminate"))
        }
    };
    pages_pdf(&[page(&content)], &[])
}

#[test]
fn a_deletion_under_a_coloured_box_is_not_a_redaction() {
    let base = may_not_terminate(None);
    for (case, paint) in [("red", "1 0 0 rg"), ("grey", "0.2 g"), ("translucent black", "/Half gs 0 g")] {
        let inspection = run(&base, &may_not_terminate(Some(paint))).unwrap();
        // The text left is a subsequence of the base: only the box can tell.
        assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{case}");
        assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{case}");
        assert!(inspection.redaction_areas.is_empty(), "{case}");
    }
}

#[test]
fn the_same_deletion_under_a_black_box_is_reported_as_a_redaction_there() {
    let base = may_not_terminate(None);
    let inspection = run(&base, &may_not_terminate(Some("0 g"))).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0);
    assert_eq!(inspection.redaction_areas.len(), 1, "{:?}", inspection.redaction_areas);
    let (page, area) = inspection.redaction_areas[0];
    assert_eq!(page, 1);
    let x0 = 20.0 + width_of("Buyer may ") - 0.5;
    for (got, want) in area.iter().zip([x0, 146.0, x0 + width_of("not") + 1.0, 162.0]) {
        assert!((got - want).abs() < 0.5, "{area:?}");
    }
}

// ── 10. encryption ──────────────────────────────────────────────────────────────────────────

#[test]
fn an_encrypted_base_is_the_base_s_problem() {
    let edited = fixture(1);
    assert!(matches!(run(&encrypted(""), &edited), Err(InspectError::Base(_))));
    assert!(matches!(run(&encrypted("pw"), &edited), Err(InspectError::Base(_))));
}

#[test]
fn a_file_that_only_mentions_encrypt_metadata_is_not_encrypted() {
    let base = fixture(1);
    let saved = update(&base, &[(7, b"<< /EncryptMetadata false /Encrypt (not a dictionary) >>".to_vec())], 8);
    let inspection = run(&base, &saved).unwrap();
    assert_eq!(
        inspection,
        Inspection { base_kept: true, pages: PageChanges { before: 1, after: 1, ..PageChanges::default() }, ..Inspection::default() }
    );
}

// ── 11. bounds ──────────────────────────────────────────────────────────────────────────────

/// The reviewer's first file: 2 pages, each drawing a 10,000 × 10,000 Flate image four times
/// (about 100 KB on disk), as a rewrite of a 2-page base.
pub(super) fn huge_images() -> (Vec<u8>, Vec<u8>) {
    let pixels = vec![0u8; 10_000 * 10_000];
    let big = flate_stream("/Type /XObject /Subtype /Image /Width 10000 /Height 10000 /ColorSpace /DeviceGray /BitsPerComponent 8", &pixels);
    let draws = "q 50 0 0 50 0 0 cm /Im1 Do Q q 50 0 0 50 50 0 cm /Im1 Do Q q 50 0 0 50 100 0 cm /Im1 Do Q q 50 0 0 50 150 0 cm /Im1 Do Q";
    (fixture(2), pages_pdf(&[page(draws), page(draws)], &[big]))
}

/// The reviewer's second file: 10 pages, each drawing one 2,000 × 2,000 image 64 times.
pub(super) fn many_draws() -> (Vec<u8>, Vec<u8>) {
    let pixels = vec![0u8; 2_000 * 2_000];
    let img = flate_stream("/Type /XObject /Subtype /Image /Width 2000 /Height 2000 /ColorSpace /DeviceGray /BitsPerComponent 8", &pixels);
    let draws: String = (0..64).map(|i| format!("q 20 0 0 20 {} {} cm /Im1 Do Q\n", (i % 8) * 25, (i / 8) * 37)).collect();
    let pages: Vec<PageSpec> = (0..10).map(|_| page(&draws)).collect();
    (fixture(10), pages_pdf(&pages, &[img]))
}

/// The reviewer's third file: 20 pages sharing one content stream of about 5 MB.
pub(super) fn shared_content() -> (Vec<u8>, Vec<u8>) {
    let mut content = Vec::new();
    let mut state = 0x9e37_79b9u32;
    while content.len() < 5_000_000 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        content.extend_from_slice(format!("{} {} 1 1 re f\n", state % 200, (state >> 8) % 300).as_bytes());
    }
    let shared = flate_stream("", &content);
    let kids: Vec<String> = (0..20).map(|i| format!("{} 0 R", 5 + i)).collect();
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count 20 /MediaBox [0 0 200 300] >>", kids.join(" ")).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        shared,
    ];
    for _ in 0..20 {
        bodies.push(b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << >> >>".to_vec());
    }
    (fixture(20), pdf(&bodies))
}

#[test]
fn the_reviewer_s_costly_files_stop_within_their_bounds() {
    let tight = Limits { deadline: Duration::from_secs(2), ..Limits::default() };
    for (case, (base, edited)) in [("huge images", huge_images()), ("many draws", many_draws()), ("shared content", shared_content())] {
        let started = Instant::now();
        let result = inspect_with(&base, &edited, tight);
        assert_eq!(result, Err(InspectError::TooComplex), "{case}");
        assert!(started.elapsed() < Duration::from_secs(10), "{case}: {:?}", started.elapsed());
    }
}

#[test]
fn work_bounds_stop_with_too_complex() {
    let base = pages_pdf(&[page(&format!("{}/Im1 Do", text_line(20.0, 150.0, "Alpha")))], &[image(10)]);
    let edited = pages_pdf(&[page(&format!("{}/Im1 Do", text_line(20.0, 150.0, "Alpha Bravo")))], &[image(10)]);
    let cases = [
        ("content bytes", Limits { content_bytes: 16, ..Limits::default() }),
        ("hashed bytes", Limits { hashed_bytes: 8, ..Limits::default() }),
        ("rendered pixels", Limits { rendered_pixels: 1_000, ..Limits::default() }),
        ("deadline", Limits { deadline: Duration::ZERO, ..Limits::default() }),
    ];
    for (case, limits) in cases {
        assert_eq!(inspect_with(&base, &edited, limits), Err(InspectError::TooComplex), "{case}");
    }
    // Forms nested deeper than the bound.
    let mut extras = Vec::new();
    for level in 0..20 {
        extras.push(stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 1 1] /Resources << /XObject << /F {} 0 R >> >>", 5 + level), b"/F Do"));
    }
    let nested = pages_pdf(&[page("/Im1 Do")], &extras);
    assert_eq!(run(&nested, &nested), Err(InspectError::TooComplex));
}

#[test]
fn pages_of_millions_of_operations_stop_on_the_work_budget_not_the_clock() {
    // Within the 4 MiB content bound: two million path operators, or 300,000 small fills.
    let no_clock = Limits { deadline: Duration::from_secs(3_600), ..Limits::default() };
    for (case, content) in [("closepaths", "h\n".repeat(2_000_000)), ("fills", "1 1 1 1 re f\n".repeat(300_000))] {
        let file = pages_pdf(&[page(&content)], &[]);
        let started = Instant::now();
        assert_eq!(inspect_with(&file, &file, no_clock), Err(InspectError::TooComplex), "{case}");
        assert!(started.elapsed() < Duration::from_secs(10), "{case}: {:?}", started.elapsed());
    }
}

#[test]
fn two_thousand_pages_compare_within_the_work_bounds() {
    let file = fixture(MAX_PAGES);
    let inspection = inspect_with(&file, &file, Limits { deadline: Duration::from_secs(3_600), ..Limits::default() }).unwrap();
    assert_eq!(inspection.pages.before, MAX_PAGES as u32);
}

// ── 12. never panics, on a valid file broken in many ways ───────────────────────────────────

#[test]
fn never_panics_on_damaged_copies_of_a_valid_edited_file() {
    let base = pages_pdf(
        &[page(&format!("{}q 40 0 0 40 20 20 cm /Im1 Do Q", text_line(20.0, 150.0, "Alpha Bravo"))), page(&text_line(20.0, 150.0, "Charlie"))],
        &[image(90)],
    );
    let saved = session_edit(
        base.clone(),
        vec![annotation(0, Shape::Ink { strokes: vec![vec![[10.0, 10.0], [60.0, 60.0]]] }), Edit::RotatePages { pages: vec![1], degrees: 90 }],
    );
    let redacted = session_edit(saved.clone(), vec![annotation(0, redact([18.0, 144.0, 61.0, 163.0])), Edit::ApplyRedactions { pages: None }]);
    let quick = Limits { render_side: 200, ..Limits::default() };
    let mut state = 0x1234_5678_9abc_def1u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut cases = 0;
    for edited in [&saved, &redacted] {
        for _ in 0..1_000 {
            let mut damaged = edited.clone();
            let at = (next() % damaged.len() as u64) as usize;
            if next() % 4 == 0 {
                damaged.truncate(at);
            } else {
                damaged[at] ^= 1u8 << (next() % 8);
            }
            let result = inspect_with(&base, &damaged, quick);
            assert!(!matches!(&result, Err(InspectError::Edited(m)) if m.starts_with("internal error")), "{result:?} at {at}");
            cases += 1;
        }
    }
    assert!(cases >= 2_000);
}

// ── extra: document-level changes and layers ────────────────────────────────────────────────

#[test]
fn refuses_a_document_level_change_it_does_not_report() {
    let base = fixture(1);
    let catalog = b"<< /Type /Catalog /Pages 2 0 R /OpenAction << /S /JavaScript /JS (app.alert(1)) >> >>".to_vec();
    let message = edited_error(run(&base, &update(&base, &[(1, catalog)], 6)));
    assert!(message.contains("OpenAction"), "{message}");
}
