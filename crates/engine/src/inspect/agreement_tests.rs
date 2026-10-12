//! Embed build: the two readers must read one document. `pdfcraft_cos` gives the structure and
//! hayro the glyphs, images and pixels; a file the two resolve differently (another object for
//! a reference, another stream extent) is refused, since its facts would describe two documents.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// A file assembled piece by piece, keeping where each object starts.
#[derive(Default)]
struct Builder {
    out: Vec<u8>,
    /// Object number → offset of its last `N 0 obj`.
    offsets: BTreeMap<u32, usize>,
}

impl Builder {
    fn new() -> Self {
        Self { out: b"%PDF-1.7\n".to_vec(), offsets: BTreeMap::new() }
    }

    /// Append `N 0 obj body endobj`; returns where it starts.
    fn object(&mut self, num: u32, body: &[u8]) -> usize {
        let at = self.out.len();
        self.offsets.insert(num, at);
        self.out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        self.out.extend_from_slice(body);
        self.out.extend_from_slice(b"\nendobj\n");
        at
    }

    /// A classic table `0..size` from `rows` (object → offset; others free), the trailer with
    /// `extra` entries, `startxref` and `%%EOF`.
    fn finish(mut self, size: u32, rows: &BTreeMap<u32, usize>, extra: &str) -> Vec<u8> {
        let xref = self.out.len();
        self.out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for num in 1..size {
            match rows.get(&num) {
                Some(offset) => self.out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes()),
                None => self.out.extend_from_slice(b"0000000000 00001 f \n"),
            }
        }
        self.out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R {extra} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        self.out
    }
}

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

const RESOURCES: &str = "/Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>";

fn text(word: &str) -> Vec<u8> {
    format!("BT /F1 12 Tf 20 150 Td ({word}) Tj ET").into_bytes()
}

/// The catalog and page tree of a one-page file whose page is object 3.
fn head(b: &mut Builder) {
    b.object(1, b"<< /Type /Catalog /Pages 2 0 R >>");
    b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
}

/// A sound one-page file showing "Alpha".
fn sound() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    let rows = b.offsets.clone();
    b.finish(5, &rows, "")
}

/// A hybrid file: the classic table points page 3 at a stale page dictionary drawing "Alpha"
/// (object 4), while the cross-reference stream (`/XRefStm`) puts page 3 in an object stream,
/// drawing "Bravo" (object 5).
fn stale_offset_beside_an_object_stream() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    let stale = b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, &stream("", &text("Bravo")));
    let page = format!("<< /Type /Page /Parent 2 0 R /Contents 5 0 R {RESOURCES} >>");
    let objects = format!("3 0 {page}");
    let first = 4;
    b.object(6, &stream(&format!("/Type /ObjStm /N 1 /First {first}"), objects.as_bytes()));
    // The cross-reference stream: entries for 3 (in object stream 6, index 0) and for 7 itself.
    let at7 = b.out.len();
    let mut entries = vec![2u8, 0, 0, 0, 6, 0, 0];
    entries.extend_from_slice(&[1u8]);
    entries.extend_from_slice(&u32::try_from(at7).unwrap().to_be_bytes());
    entries.extend_from_slice(&[0, 0]);
    b.object(7, &stream("/Type /XRef /Size 8 /W [1 4 2] /Index [3 1 7 1] /Root 1 0 R", &entries));
    let mut rows = b.offsets.clone();
    rows.insert(3, stale);
    b.finish(8, &rows, &format!("/XRefStm {at7}"))
}

/// The content stream declares 10 bytes; its `endstream` comes after all of them.
fn length_short_of_endstream() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let mut body = b"<< /Length 10 >>\nstream\n".to_vec();
    body.extend_from_slice(&text("Alpha"));
    body.extend_from_slice(b"\nendstream");
    b.object(4, &body);
    let rows = b.offsets.clone();
    b.finish(5, &rows, "")
}

/// Object 4 is defined twice in one revision: "Alpha" first, "Bravo" second; the table points
/// at the first.
fn one_number_twice() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let first = b.object(4, &stream("", &text("Alpha")));
    b.object(4, &stream("", &text("Bravo")));
    let mut rows = b.offsets.clone();
    rows.insert(4, first);
    b.finish(5, &rows, "")
}

/// A classic table written by hand: `rows` as (first object, entries), each entry an offset or
/// `None` (free).
fn finish_rows(mut b: Builder, size: u32, sections: &[(u32, Vec<Option<usize>>)], extra: &str) -> Vec<u8> {
    let xref = b.out.len();
    b.out.extend_from_slice(b"xref\n");
    for (first, entries) in sections {
        b.out.extend_from_slice(format!("{first} {}\n", entries.len()).as_bytes());
        for entry in entries {
            match entry {
                Some(offset) => b.out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes()),
                None => b.out.extend_from_slice(b"0000000000 65535 f \n"),
            }
        }
    }
    b.out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R {extra} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    b.out
}

/// One table lists object 4 twice, in two subsections: "Alpha" first, "Bravo" second.
fn one_number_listed_twice() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let alpha = b.object(4, &stream("", &text("Alpha")));
    let bravo = b.object(4, &stream("", &text("Bravo")));
    let o = |n: u32| Some(b.offsets[&n]);
    let sections = vec![(0, vec![None, o(1), o(2), o(3), Some(alpha)]), (4, vec![Some(bravo)])];
    finish_rows(b, 5, &sections, "")
}

/// A hybrid file whose table marks page 3 free while its cross-reference stream puts it in an
/// object stream, and the stale page dictionary still in the file.
fn stale_page_free_in_table() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, &stream("", &text("Bravo")));
    let page = format!("<< /Type /Page /Parent 2 0 R /Contents 5 0 R {RESOURCES} >>");
    b.object(6, &stream("/Type /ObjStm /N 1 /First 4", format!("3 0 {page}").as_bytes()));
    let at7 = b.out.len();
    let mut entries = vec![2u8, 0, 0, 0, 6, 0, 0, 1];
    entries.extend_from_slice(&u32::try_from(at7).unwrap().to_be_bytes());
    entries.extend_from_slice(&[0, 0]);
    b.object(7, &stream("/Type /XRef /Size 8 /W [1 4 2] /Index [3 1 7 1] /Root 1 0 R", &entries));
    let o = |n: u32| Some(b.offsets[&n]);
    let sections = vec![(0, vec![None, o(1), o(2), None, o(4), o(5), o(6), o(7)])];
    finish_rows(b, 8, &sections, &format!("/XRefStm {at7}"))
}

/// A cross-reference stream whose `/Index` lists page 3 twice: in the file (the stale page) and
/// in an object stream.
fn stream_index_lists_a_number_twice() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    let stale = b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, &stream("", &text("Bravo")));
    let page = format!("<< /Type /Page /Parent 2 0 R /Contents 5 0 R {RESOURCES} >>");
    b.object(6, &stream("/Type /ObjStm /N 1 /First 4", format!("3 0 {page}").as_bytes()));
    let at7 = b.out.len();
    let row = |kind: u8, field: usize, extra: u16| {
        let mut out = vec![kind];
        out.extend_from_slice(&u32::try_from(field).unwrap().to_be_bytes());
        out.extend_from_slice(&extra.to_be_bytes());
        out
    };
    let mut entries = Vec::new();
    for (kind, field, extra) in [
        (0, 0, 65535),
        (1, b.offsets[&1], 0),
        (1, b.offsets[&2], 0),
        (1, stale, 0),
        (1, b.offsets[&4], 0),
        (1, b.offsets[&5], 0),
        (1, b.offsets[&6], 0),
        (1, at7, 0),
        (2, 6, 0),
    ] {
        entries.extend(row(kind, field, extra));
    }
    b.object(7, &stream("/Type /XRef /Size 8 /W [1 4 2] /Index [0 8 3 1] /Root 1 0 R", &entries));
    b.out.extend_from_slice(format!("startxref\n{at7}\n%%EOF\n").as_bytes());
    b.out
}

/// The content stream's `/Length` is exact, but a comment stands between its data and
/// `endstream`.
fn comment_before_endstream() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let data = text("Alpha");
    let mut body = format!("<< /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(&data);
    body.extend_from_slice(b"\n% BT /F1 12 Tf 20 100 Td (Bravo) Tj ET\nendstream");
    b.object(4, &body);
    let rows = b.offsets.clone();
    b.finish(5, &rows, "")
}

/// An image drawn with `/Length` exact but a comment before its `endstream`.
fn image_with_comment_before_endstream() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >> >>");
    b.object(4, &stream("", b"q 100 0 0 100 50 100 cm /Im Do Q"));
    let pixels = [0u8, 80, 160, 240];
    let mut body =
        b"<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 /Length 4 >>\nstream\n".to_vec();
    body.extend_from_slice(&pixels);
    body.extend_from_slice(b"\n% more samples\nendstream");
    b.object(5, &body);
    let rows = b.offsets.clone();
    b.finish(6, &rows, "")
}

fn refused(file: &[u8]) -> String {
    match inspect(&sound(), file, InspectLimits::default()) {
        Err(InspectError::Edited(message)) => message,
        other => panic!("expected Err(Edited), got {other:?}"),
    }
}

#[test]
fn a_stale_offset_beside_an_object_stream_is_refused() {
    // The cross-reference stream lists page 3 twice: at the stale page dictionary in the file,
    // and in the object stream. Each reader takes another one.
    let message = refused(&stream_index_lists_a_number_twice());
    assert!(message.contains("readers disagree"), "{message}");
}

#[test]
fn two_objects_with_one_number_in_one_revision_are_refused() {
    // One table lists object 4 in two subsections, at two definitions.
    let message = refused(&one_number_listed_twice());
    assert!(message.contains("readers disagree"), "{message}");
}

/// A page with an annotation whose appearance (object 6) is `length` and `data`, then `endstream`:
/// a stream hayro does not draw (annotations are compared, not drawn), but reads as it reads any
/// object a reference names.
fn appearance_with(length: usize, data: &[u8]) -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [5 0 R] {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, b"<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /AP << /N 6 0 R >> >>");
    let mut body = format!("<< /Type /XObject /Subtype /Form /BBox [0 0 60 60] /Length {length} >>\nstream\n").into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    b.object(6, &body);
    let rows = b.offsets.clone();
    b.finish(7, &rows, "")
}

#[test]
fn a_stream_whose_length_disagrees_with_its_endstream_is_read_only_where_both_readers_agree() {
    // `/Length` exact, a comment before `endstream`: `pdfcraft_cos` takes the declared bytes,
    // hayro (finding no `endstream` right after them) everything up to `endstream`.
    let message = refused(&comment_before_endstream());
    assert!(message.contains("readers disagree on the content of page 1"), "{message}");
    let message = refused(&image_with_comment_before_endstream());
    assert!(message.contains("readers disagree: hayro read other data for stream 5 0 R"), "{message}");
    // `/Length` short of `endstream`: both readers take the data up to `endstream`, and hayro's
    // reading is checked to be the same bytes. Tolerated in the base's own objects (as the files
    // signed by some tools have it), and compared as usual: the base against itself, and an
    // update appended to it that keeps those objects.
    let short = length_short_of_endstream();
    assert!(inspect(&short, &short, InspectLimits::default()).is_ok());
    let inspection = inspect(
        &short,
        &appended(&short, 3, &format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Rotate 90 {RESOURCES} >>")),
        InspectLimits::default(),
    )
    .unwrap();
    assert_eq!(
        (inspection.pages.rotated, inspection.text, inspection.content),
        (1, TextChanges::default(), ContentChanges::default()),
        "{inspection:?}"
    );
    // An annotation's appearance, which hayro does not draw: it reads it all the same (every
    // object `pdfcraft_cos` reads is one hayro reads), and finds the same data.
    let square = b"0 0 1 RG 2 2 56 56 re S";
    let short_appearance = appearance_with(10, square);
    assert!(inspect(&short_appearance, &short_appearance, InspectLimits::default()).is_ok());
    // The exact length and a comment before `endstream`: `pdfcraft_cos` takes the declared
    // bytes, hayro everything up to `endstream`. They disagree, on a stream nothing draws.
    let mut commented = square.to_vec();
    commented.extend_from_slice(b"\n% 0 g 0 0 60 60 re f");
    let message = refused(&appearance_with(square.len(), &commented));
    assert!(message.contains("readers disagree: hayro read other data for stream 6 0 R"), "{message}");
}

/// `base` with an update appended that replaces object `num` with `body` (the base's table has
/// five or seven rows: the update's `/Size` is the larger).
fn appended(base: &[u8], num: u32, body: &str) -> Vec<u8> {
    let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
    let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let mut out = base.to_vec();
    let offset = out.len();
    out.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \n{num} 1\n{offset:010} 00000 n \n").as_bytes());
    out.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

/// A page drawing `data` as its content, declared `length` long.
fn page_declaring(length: usize, data: &[u8]) -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let mut body = format!("<< /Length {length} >>\nstream\n").into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    b.object(4, &body);
    let rows = b.offsets.clone();
    b.finish(5, &rows, "")
}

#[test]
fn an_object_the_edited_file_brings_must_declare_its_length() {
    // R6b-30. The edited content: "Alpha", a box over the word the base shows, `% endobj`, then
    // a clause, declared 7 bytes long. `pdfcraft_cos` and hayro read to `endstream`; a reader
    // that ends the data at the first `endobj` shows no clause (and PDFium does so).
    let base = {
        let data = [text("Alpha"), b"\nBT /F1 12 Tf 150 20 Td (X) Tj ET".to_vec()].concat();
        page_declaring(data.len(), &data)
    };
    let data = [text("Alpha"), b"\n0 g 146 14 16 18 re f\n% endobj\nBT /F1 12 Tf 20 100 Td (Seller is liable) Tj ET".to_vec()].concat();
    // In a rewrite, every object is the edited file's own.
    let message = refused_against(&base, &page_declaring(7, &data));
    assert!(message.contains("does not have the length it declares"), "{message}");
    // In an appended update, a new object (here, the page's content written again).
    let mut body = b"<< /Length 7 >>\nstream\n".to_vec();
    body.extend_from_slice(&data);
    body.extend_from_slice(b"\nendstream");
    let message = refused_against(&base, &appended(&base, 4, &String::from_utf8(body).unwrap()));
    assert!(message.contains("does not have the length it declares"), "{message}");
    // The same rewrite with the exact length is read like any other.
    assert!(inspect(&base, &page_declaring(data.len(), &data), InspectLimits::default()).is_ok());
    // A base object whose length is wrong, written again by a rewrite with that wrong length.
    let short = length_short_of_endstream();
    let message = refused_against(&sound(), &short);
    assert!(message.contains("does not have the length it declares"), "{message}");
    let square = b"0 0 1 RG 2 2 56 56 re S";
    let message = refused_against(&appearance_with(square.len(), square), &appearance_with(10, square));
    assert!(message.contains("stream 6 0 R does not have the length it declares"), "{message}");
}

/// `base` with a cross-reference section appended that lists object `num` at `offset`, and
/// nothing else: no object is written.
fn repointed(base: &[u8], num: u32, offset: usize) -> Vec<u8> {
    let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
    let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let mut out = base.to_vec();
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \n{num} 1\n{offset:010} 00000 n \n").as_bytes());
    out.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

/// Where `N 0 obj` first starts in `file`.
fn object_offset(file: &[u8], num: u32) -> usize {
    let header = format!("\n{num} 0 obj\n");
    file.windows(header.len()).position(|w| w == header.as_bytes()).unwrap() + 1
}

#[test]
fn an_update_pointing_an_object_back_at_an_older_misdeclared_copy_is_refused() {
    // R6b-35, the reviewer's C1 (`n2_resurrected_base_bytes`). The base has two revisions: in
    // the first, the content (object 4) declares 7 bytes though its data runs on to `endstream`
    // through `% endobj` and a clause; the second writes it again with its exact length. The
    // edited file appends one table, and nothing else, that points object 4 back at the first
    // copy: still in the base's bytes, but not an object of the base revision (the base never
    // read it). PDFium ends that data at `endobj` and drops the clause; on e91797d this was
    // reported as unchanged.
    let data = [text("Alpha"), b"\nBT /F1 12 Tf 150 20 Td (X) Tj ET\n% endobj\nBT /F1 12 Tf 20 100 Td (Seller is liable) Tj ET".to_vec()].concat();
    let first = page_declaring(7, &data);
    let mut exact = format!("<< /Length {} >>\nstream\n", data.len()).into_bytes();
    exact.extend_from_slice(&data);
    exact.extend_from_slice(b"\nendstream");
    let base = appended(&first, 4, &String::from_utf8(exact).unwrap());
    assert!(inspect(&base, &base, InspectLimits::default()).is_ok_and(|inspection| inspection.content == ContentChanges::default()));
    let resurrected = repointed(&base, 4, object_offset(&first, 4));
    let message = refused_against(&base, &resurrected);
    assert!(message.contains("stream 4 0 R does not have the length it declares"), "{message}");
    // A base whose own newest revision has the misdeclared object: an update that lists it again
    // at the offset the base names for it keeps the base's own object, and is read as before.
    let short = length_short_of_endstream();
    let kept = repointed(&short, 4, object_offset(&short, 4));
    let inspection = inspect(&short, &kept, InspectLimits::default()).unwrap();
    assert!(inspection.base_kept, "{inspection:?}");
    assert_eq!((inspection.text, inspection.content), (TextChanges::default(), ContentChanges::default()), "{inspection:?}");
}

#[test]
fn the_editor_writes_every_length_exactly() {
    // A base whose content declares a short length: the editor's appended update keeps it, and
    // its rewrite (an applied redaction) writes the content again with its exact length.
    use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};
    let short = length_short_of_endstream();
    let save = |edits: Vec<Edit>| {
        let mut session = Session::new().with_clock(|| 1_700_000_000);
        let id = session.open("base.pdf", None, std::sync::Arc::new(short.clone()), None).unwrap();
        for edit in edits {
            session.apply(id, edit).unwrap();
        }
        session.save_bytes(id).unwrap().as_ref().clone()
    };
    let rotated = inspect(&short, &save(vec![Edit::RotatePages { pages: vec![0], degrees: 90 }]), InspectLimits::default()).unwrap();
    assert!(rotated.base_kept && rotated.pages.rotated == 1, "{rotated:?}");
    let mark = Shape::Redact { quads: vec![rect_quad([18.0, 144.0, 61.0, 163.0])], overlay: String::new(), look: Default::default() };
    let redact =
        Edit::AddAnnotation(NewAnnotation { page: 0, style: Style::default_for(&mark), shape: mark, contents: String::new(), author: "Sari".into() });
    let redacted = inspect(&short, &save(vec![redact, Edit::ApplyRedactions { pages: None }]), InspectLimits::default()).unwrap();
    assert!(!redacted.base_kept, "{redacted:?}");
    assert_eq!(redacted.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{redacted:?}");
    assert_eq!(redacted.content.pages_changed_outside_redactions, 0, "{redacted:?}");
}

fn refused_against(base: &[u8], file: &[u8]) -> String {
    match inspect(base, file, InspectLimits::default()) {
        Err(InspectError::Edited(message)) => message,
        other => panic!("expected Err(Edited), got {other:?}"),
    }
}

#[test]
fn files_both_readers_read_alike_are_not_refused() {
    // A second definition of object 4 that the table does not list; a hybrid file whose table
    // lists page 3 (stale or not) where its cross-reference stream also does, or marks it free.
    // The readers agree on each; as rewrites, each is refused only for what it carries beside
    // what it lists: the second definition, or a stale page listed free, no cross-reference data
    // lists (follow-up 7 audit); the stale page and its content, which nothing reaches (R6b-48).
    let message = refused(&one_number_twice());
    assert!(message.contains("bytes no cross-reference data lists"), "{message}");
    for (case, file) in [("hybrid, table first", stale_offset_beside_an_object_stream()), ("hybrid, free in table", stale_page_free_in_table())] {
        let message = refused(&file);
        assert!(message.contains("nothing reaches") || message.contains("no cross-reference data lists"), "{case}: {message}");
    }
}

// ── audit of the appended-update path ───────────────────────────────────────────────────────

/// An object an update writes: its number and body.
type Written<'a> = (u32, &'a [u8]);

/// `base` with an update appended holding `objects` (number, body), whose section has `/Prev`
/// `prev` (`None`: none) and the trailer entries `extra`.
fn update_section(base: &[u8], objects: &[(u32, &[u8])], prev: Option<usize>, extra: &str) -> Vec<u8> {
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
    let prev = prev.map(|prev| format!("/Prev {prev}")).unwrap_or_default();
    // At least the base's /Size: an edited file reads no object at or above its own.
    let at = base.windows(6).rposition(|w| w == b"/Size ").unwrap_or(0);
    let base_size: u32 = std::str::from_utf8(&base[at + 6..]).ok().and_then(|rest| rest.split_whitespace().next()?.parse().ok()).unwrap_or(0);
    let size = base_size.max(9);
    out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R {prev} {extra} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

/// The offset `startxref` names in `file`.
fn last_section(file: &[u8]) -> usize {
    let at = file.windows(9).rposition(|w| w == b"startxref").unwrap();
    std::str::from_utf8(&file[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap()
}

#[test]
fn an_update_whose_chain_does_not_reach_the_base_s_last_section_is_refused() {
    // The edited file starts with the base's bytes, but its newest section does not lead back to
    // the base's newest section at exactly its offset: not an appended update of this base.
    let first = sound();
    let base = appended(&first, 4, &String::from_utf8(stream("", &text("Bravo"))).unwrap());
    let page = format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Rotate 90 {RESOURCES} >>");
    let page = page.as_bytes();
    // (case, /Prev)
    for (case, prev) in [
        ("no /Prev", None),
        ("the base's older section, skipping its newest", Some(last_section(&first))),
        ("an offset inside the base that is no section", Some(object_offset(&base, 3))),
    ] {
        let edited = update_section(&base, &[(3, page)], prev, "");
        assert!(matches!(inspect(&base, &edited, InspectLimits::default()), Err(InspectError::Edited(_))), "{case}");
    }
    // Chained onto the base's newest section: a rotation.
    let edited = update_section(&base, &[(3, page)], Some(last_section(&base)), "");
    let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert!(inspection.base_kept && inspection.pages.rotated == 1, "{inspection:?}");
}

#[test]
fn an_update_that_writes_an_object_nothing_compares_is_refused() {
    // Every object an appended update writes must be one the comparison reads, so compared
    // under the appended-update rules. On e91797d each of these was reported as unchanged.
    let mut b = Builder::new();
    b.object(1, b"<< /Type /Catalog /Pages 2 0 R /Metadata 5 0 R /StructTreeRoot 7 0 R >>");
    b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, &stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>"));
    b.object(6, b"<< /Title (Agreement) >>");
    b.object(7, b"<< /Type /StructTreeRoot >>");
    let rows = b.offsets.clone();
    let base = b.finish(8, &rows, "/Info 6 0 R");
    let prev = Some(last_section(&base));
    let metadata = stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><other/></x:xmpmeta>");
    // The page drawing the same content from a new object, and its old content object written
    // again with other data: the page is unchanged, the old object read by nothing.
    let page = format!("<< /Type /Page /Parent 2 0 R /Contents 8 0 R {RESOURCES} >>").into_bytes();
    let content = stream("", &text("Alpha"));
    let cases: [(&str, Vec<Written<'_>>); 4] = [
        ("an object nothing refers to", vec![(8, b"<< /Note (hidden) >>")]),
        ("a base object nothing refers to any more", vec![(3, &page), (8, &content), (4, b"<< /Note (hidden) >>")]),
        ("the XMP metadata", vec![(5, &metadata)]),
        ("the structure tree", vec![(7, b"<< /Type /StructTreeRoot /K [] >>")]),
    ];
    for (case, objects) in cases {
        let edited = update_section(&base, &objects, prev, "/Info 6 0 R");
        // (The metadata and the structure tree are read and compared now: refused as changed.)
        let expected = match case {
            "the XMP metadata" => "/Metadata changed",
            "the structure tree" => "structure tree",
            _ => "nothing this comparison reports reads",
        };
        match inspect(&base, &edited, InspectLimits::default()) {
            Err(InspectError::Edited(message)) => assert!(message.contains(expected), "{case}: {message}"),
            other => panic!("{case}: {other:?}"),
        }
    }
    // The document information, which is read for a fact (R6b-42): reported, not refused.
    let edited = update_section(&base, &[(6, b"<< /Title (Agreement) /ModDate (D:20231114221320Z) >>")], prev, "/Info 6 0 R");
    let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!((inspection.text, inspection.content), (TextChanges::default(), ContentChanges::default()), "{inspection:?}");
    assert_eq!(inspection.info.changed, ["ModDate"], "{inspection:?}");
}

/// A one-page file showing "Alpha" and, unless `boxed`, a word at (150, 20) (with `boxed`, a
/// black box over where it was), whose catalog has `catalog` entries and the objects `extra`
/// (6 onwards).
fn tagged(boxed: bool, catalog: &str, extra: &[&[u8]]) -> Vec<u8> {
    let mut b = Builder::new();
    b.object(1, format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>").as_bytes());
    b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let last = if boxed { "0 g 146 14 16 18 re f" } else { "BT /F1 12 Tf 150 20 Td (X) Tj ET" };
    b.object(4, &stream("", &[text("Alpha"), b"\n".to_vec(), last.as_bytes().to_vec()].concat()));
    b.object(5, b"null");
    for (i, body) in extra.iter().enumerate() {
        b.object(6 + i as u32, body);
    }
    let rows = b.offsets.clone();
    b.finish(6 + extra.len() as u32, &rows, "")
}

#[test]
fn the_structure_tree_s_text_may_only_be_taken_away_by_a_rewrite() {
    // The tags' text is what assistive technology reads and what copying gives: a rewrite (a
    // redaction) may take some away, as the redactor cleans the tags of what it removed, but
    // never add or change any; an appended update may not change the tree at all. On e91797d
    // the structure tree was not compared at all.
    let tree = |alt: &str| -> Vec<Vec<u8>> {
        vec![b"<< /Type /StructTreeRoot /K 7 0 R >>".to_vec(), format!("<< /S /Figure /P 6 0 R /Pg 3 0 R {alt} /K 0 >>").into_bytes()]
    };
    let file = |boxed: bool, alt: &str| {
        let objects = tree(alt);
        tagged(boxed, "/StructTreeRoot 6 0 R /MarkInfo << /Marked true >>", &objects.iter().map(Vec::as_slice).collect::<Vec<_>>())
    };
    let base = file(false, "/Alt (A signature)");
    assert!(inspect(&base, &base, InspectLimits::default()).is_ok());
    // (case, the element's text entries in the rewrite, accepted)
    for (case, alt, accepted) in [
        ("as it was", "/Alt (A signature)", true),
        ("taken away", "", true),
        ("changed", "/Alt (No signature)", false),
        ("added", "/Alt (A signature) /ActualText (Paid)", false),
        ("a language added", "/Alt (A signature) /Lang (en)", false),
    ] {
        let result = inspect(&base, &file(true, alt), InspectLimits::default());
        if accepted {
            let inspection = result.unwrap_or_else(|error| panic!("{case}: {error:?}"));
            assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{case}: {inspection:?}");
        } else {
            match result {
                Err(InspectError::Edited(message)) => assert!(message.contains("structure tree"), "{case}: {message}"),
                other => panic!("{case}: {other:?}"),
            }
        }
    }
    // An appended update may not change the tree, its text or anything else in it.
    let prev = Some(last_section(&base));
    for (case, element) in [
        ("text taken away", "<< /S /Figure /P 6 0 R /Pg 3 0 R /K 0 >>"),
        ("another role", "<< /S /Span /P 6 0 R /Pg 3 0 R /Alt (A signature) /K 0 >>"),
    ] {
        let edited = update_section(&base, &[(7, element.as_bytes())], prev, "");
        match inspect(&base, &edited, InspectLimits::default()) {
            Err(InspectError::Edited(message)) => assert!(message.contains("structure tree"), "{case}: {message}"),
            other => panic!("{case}: {other:?}"),
        }
    }
}

#[test]
fn the_version_mark_info_and_metadata_are_compared() {
    // `/Version` and `/MarkInfo` may not change, nor `/Metadata`, which a rewrite may remove
    // (an editor's sanitising does) but not change. On e91797d all three were left out.
    let xmp = |title: &str| stream("/Type /Metadata /Subtype /XML", format!("<x:xmpmeta xmlns:x='adobe:ns:meta/'>{title}</x:xmpmeta>").as_bytes());
    let file = |boxed: bool, catalog: &str, title: &str| tagged(boxed, catalog, &[&xmp(title)]);
    let base = file(false, "/Version /1.7 /MarkInfo << /Marked true >> /Metadata 6 0 R", "Agreement");
    // (case, catalog, title, accepted)
    for (case, catalog, title, accepted) in [
        ("as it was", "/Version /1.7 /MarkInfo << /Marked true >> /Metadata 6 0 R", "Agreement", true),
        ("the metadata removed", "/Version /1.7 /MarkInfo << /Marked true >>", "Agreement", true),
        ("the metadata changed", "/Version /1.7 /MarkInfo << /Marked true >> /Metadata 6 0 R", "Another", false),
        ("another version", "/Version /2.0 /MarkInfo << /Marked true >> /Metadata 6 0 R", "Agreement", false),
        ("marked otherwise", "/Version /1.7 /MarkInfo << /Marked true /Suspects true >> /Metadata 6 0 R", "Agreement", false),
    ] {
        // The metadata removed leaves it null: a rewrite carries nothing nothing reaches (R6b-48).
        let edited = if catalog.contains("/Metadata") { file(true, catalog, title) } else { tagged(true, catalog, &[b"null"]) };
        let result = inspect(&base, &edited, InspectLimits::default());
        match (accepted, result) {
            (true, Ok(inspection)) => assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{case}"),
            (false, Err(InspectError::Edited(message))) => assert!(message.contains("changed"), "{case}: {message}"),
            (_, other) => panic!("{case}: {other:?}"),
        }
    }
    // In an appended update, the metadata removed is a change.
    let prev = Some(last_section(&base));
    let catalog = "<< /Type /Catalog /Pages 2 0 R /Version /1.7 /MarkInfo << /Marked true >> >>".to_string();
    let edited = update_section(&base, &[(1, catalog.as_bytes())], prev, "");
    assert!(matches!(inspect(&base, &edited, InspectLimits::default()), Err(InspectError::Edited(_))));
}

/// The dictionaries of [`with_every_dictionary`], by name: (name, object number).
const DICTIONARIES: [(&str, u32); 12] = [
    ("catalog", 1),
    ("page", 3),
    ("resources", 5),
    ("font", 6),
    ("form XObject", 7),
    ("graphics state", 8),
    ("pattern", 9),
    ("annotation", 10),
    ("shading", 11),
    ("outline item", 21),
    ("name tree", 22),
    ("form field", 30),
];

/// One page drawing with a font, a form XObject, a graphics state, a shading pattern and a
/// shading, with an annotation, a form field, an outline item and a named destination, and a
/// word (`boxed`: a black box over it instead). Dictionary `zz` (an object number, see
/// [`DICTIONARIES`]) has an entry no comparison knows, `/Zz 1`.
fn with_every_dictionary(boxed: bool, zz: Option<u32>) -> Vec<u8> {
    let extra = |num: u32| if zz == Some(num) { " /Zz 1" } else { "" };
    let content = format!(
        "q /GS gs 0 G 10 10 m 30 10 l S Q\n/Fm Do\nq 20 100 60 30 re W n /Sh sh Q\nq /Pattern cs /P1 scn 100 100 60 30 re f Q\n{}\n{}",
        String::from_utf8(text("Alpha")).unwrap(),
        if boxed { "0 g 146 14 16 18 re f" } else { "BT /F1 12 Tf 150 20 Td (X) Tj ET" }
    );
    let function = "<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>";
    let objects: Vec<(u32, Vec<u8>)> = vec![
        (1, format!("<< /Type /Catalog /Pages 2 0 R /Outlines 20 0 R /Names << /Dests 22 0 R >> /AcroForm << /Fields [30 0 R] >>{} >>", extra(1)).into_bytes()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec()),
        (3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources 5 0 R /Annots [10 0 R 30 0 R]{} >>", extra(3)).into_bytes()),
        (4, stream("", content.as_bytes())),
        (
            5,
            format!("<< /Font << /F1 6 0 R >> /XObject << /Fm 7 0 R >> /ExtGState << /GS 8 0 R >> /Pattern << /P1 9 0 R >> /Shading << /Sh 11 0 R >>{} >>", extra(5))
                .into_bytes(),
        ),
        (6, format!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica{} >>", extra(6)).into_bytes()),
        (7, stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 200 300]{}", extra(7)), b"0 0 1 rg 20 250 20 20 re f")),
        (8, format!("<< /Type /ExtGState /LW 2{} >>", extra(8)).into_bytes()),
        (9, format!("<< /Type /Pattern /PatternType 2 /Shading 12 0 R{} >>", extra(9)).into_bytes()),
        (10, format!("<< /Type /Annot /Subtype /Square /Rect [120 200 180 260] /C [1 0 0]{} >>", extra(10)).into_bytes()),
        (11, format!("<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 200 0] /Function {function}{} >>", extra(11)).into_bytes()),
        (12, format!("<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 200 0] /Function {function} >>").into_bytes()),
        (20, b"<< /Type /Outlines /First 21 0 R /Last 21 0 R /Count 1 >>".to_vec()),
        (21, format!("<< /Title (Terms) /Parent 20 0 R /Dest [3 0 R /Fit]{} >>", extra(21)).into_bytes()),
        (22, format!("<< /Names [(terms) [3 0 R /Fit]]{} >>", extra(22)).into_bytes()),
        (30, format!("<< /FT /Tx /T (name) /V (Sari) /Type /Annot /Subtype /Widget /Rect [20 40 120 60] /P 3 0 R{} >>", extra(30)).into_bytes()),
    ];
    let mut b = Builder::new();
    for (num, body) in &objects {
        b.object(*num, body);
    }
    let rows = b.offsets.clone();
    b.finish(31, &rows, "")
}

#[test]
fn an_unknown_key_in_any_dictionary_is_refused_or_reported() {
    // The completeness audit: every key of every dictionary the comparison reads is compared,
    // ignored for a stated reason, or any change to it is a change. A key nobody listed, added
    // to each kind of dictionary, in a rewrite (with a redaction) and in an appended update.
    let base = with_every_dictionary(false, None);
    let clean = inspect(&base, &base, InspectLimits::default()).unwrap();
    assert_eq!(clean, Inspection { base_kept: true, pages: PageChanges { before: 1, after: 1, ..PageChanges::default() }, ..Inspection::default() });
    let redacted = inspect(&base, &with_every_dictionary(true, None), InspectLimits::default()).unwrap();
    assert_eq!((redacted.content.pages_changed_outside_redactions, redacted.redaction_areas.len()), (0, 1), "{redacted:?}");
    // Refused, or a change reported.
    let caught = |result: Result<Inspection, InspectError>| match result {
        Err(InspectError::Edited(_)) => true,
        Ok(inspection) => {
            inspection.content.pages_with_content_changed > 0
                || inspection.content.pages_changed_outside_redactions > 0
                || inspection.form_fields_changed > 0
                || inspection.annotations != AnnotationChanges::default()
        }
        Err(_) => false,
    };
    let prev = Some(last_section(&base));
    for (name, num) in DICTIONARIES {
        let rewrite = with_every_dictionary(true, Some(num));
        assert!(
            caught(inspect(&base, &rewrite, InspectLimits::default())),
            "{name}, in a rewrite: {:?}",
            inspect(&base, &rewrite, InspectLimits::default())
        );
        // The same object in an appended update: its body as the rewrite has it.
        let marked = with_every_dictionary(false, Some(num));
        let start = object_offset(&marked, num);
        let body_start = start + format!("{num} 0 obj\n").len();
        let end = body_start + marked[body_start..].windows(8).position(|w| w == b"\nendobj\n").unwrap();
        let appended = update_section(&base, &[(num, &marked[body_start..end])], prev, "");
        assert!(
            caught(inspect(&base, &appended, InspectLimits::default())),
            "{name}, appended: {:?}",
            inspect(&base, &appended, InspectLimits::default())
        );
    }
}

/// The objects of a two-page file whose ordered structures can each be reordered: `swap` names
/// the one reordered (see [`ORDERS`]); `boxed` puts a black box over a word instead of it.
fn ordered_objects(boxed: bool, swap: &str) -> Vec<(u32, Vec<u8>)> {
    let pick = |name: &str, plain: &'static str, swapped: &'static str| if swap == name { swapped } else { plain };
    let content = format!(
        "{}\nq /GS gs 0 G 20 60 m 180 60 l S Q\n{}",
        String::from_utf8(text("Alpha")).unwrap(),
        if boxed { "0 g 146 14 16 18 re f" } else { "BT /F1 12 Tf 150 20 Td (X) Tj ET" }
    );
    let widget = |name: &str, y: u32, extra: &str| {
        format!("<< /T ({name}) /FT /Tx /Type /Annot /Subtype /Widget /Rect [20 {y} 120 {}] /P 3 0 R {extra} >>", y + 15).into_bytes()
    };
    let outline = swap == "outline";
    vec![
        (
            1,
            format!(
                "<< /Type /Catalog /Pages 2 0 R /Outlines 20 0 R /Names << /Dests 22 0 R >> /AcroForm << /Fields [{}] /CO [{}] >> /OCProperties << /OCGs [40 0 R 41 0 R] /D << /ON [{}] /Order [{}] >> >> >>",
                pick("fields", "30 0 R 31 0 R 32 0 R", "31 0 R 30 0 R 32 0 R"),
                pick("calculation", "30 0 R 31 0 R", "31 0 R 30 0 R"),
                pick("on", "40 0 R 41 0 R", "41 0 R 40 0 R"),
                pick("order", "40 0 R 41 0 R", "41 0 R 40 0 R"),
            )
            .into_bytes(),
        ),
        (2, format!("<< /Type /Pages /Kids [{}] /Count 2 /MediaBox [0 0 200 300] >>", pick("pages", "3 0 R 50 0 R", "50 0 R 3 0 R")).into_bytes()),
        (
            3,
            format!(
                "<< /Type /Page /Parent 2 0 R /Contents [{}] /Annots [{}] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> /ExtGState << /GS 8 0 R >> >> >>",
                pick("contents", "4 0 R 13 0 R", "13 0 R 4 0 R"),
                pick("annots", "10 0 R 30 0 R 31 0 R 33 0 R 34 0 R", "30 0 R 10 0 R 31 0 R 33 0 R 34 0 R"),
            )
            .into_bytes(),
        ),
        (4, stream("", content.as_bytes())),
        (13, stream("", b"BT /F1 12 Tf 20 100 Td (Bravo) Tj ET")),
        (8, format!("<< /Type /ExtGState /LW 2 /D [{} 0] >>", pick("dash", "[6 2]", "[2 6]")).into_bytes()),
        (10, b"<< /Type /Annot /Subtype /Square /Rect [20 180 80 240] /C [1 0 0] >>".to_vec()),
        (20, format!("<< /Type /Outlines /First {} 0 R /Last {} 0 R /Count 2 >>", if outline { 23 } else { 21 }, if outline { 21 } else { 23 }).into_bytes()),
        (21, format!("<< /Title (Terms) /Parent 20 0 R {} /Dest [3 0 R /Fit] >>", if outline { "/Prev 23 0 R" } else { "/Next 23 0 R" }).into_bytes()),
        (23, format!("<< /Title (Price) /Parent 20 0 R {} /Dest [50 0 R /Fit] >>", if outline { "/Next 21 0 R" } else { "/Prev 21 0 R" }).into_bytes()),
        (
            22,
            format!("<< /Names [{}] >>", pick("names", "(price) [50 0 R /Fit] (terms) [3 0 R /Fit]", "(terms) [3 0 R /Fit] (price) [50 0 R /Fit]")).into_bytes(),
        ),
        (30, widget("name", 260, "")),
        (31, format!("<< /T (choice) /FT /Ch /Opt [{}] /Type /Annot /Subtype /Widget /Rect [130 260 190 275] /P 3 0 R >>", pick("options", "(a) (b)", "(b) (a)")).into_bytes()),
        (32, format!("<< /T (group) /Kids [{}] >>", pick("kids", "33 0 R 34 0 R", "34 0 R 33 0 R")).into_bytes()),
        (33, widget("first", 240, "/Parent 32 0 R")),
        (34, widget("second", 220, "/Parent 32 0 R")),
        (40, b"<< /Type /OCG /Name (One) >>".to_vec()),
        (41, b"<< /Type /OCG /Name (Two) >>".to_vec()),
        (50, b"<< /Type /Page /Parent 2 0 R /Contents 51 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>".to_vec()),
        (51, stream("", b"BT /F1 12 Tf 20 150 Td (Page two) Tj ET")),
    ]
}

/// What [`ordered_objects`] can reorder: (name, what it is).
const ORDERS: [(&str, &str); 12] = [
    ("annots", "a page's /Annots"),
    ("contents", "a page's /Contents array"),
    ("pages", "the page tree's /Kids"),
    ("outline", "outline siblings"),
    ("names", "a name tree's entries"),
    ("dash", "a graphics state's dash array"),
    ("on", "/OCProperties /D /ON"),
    ("order", "/OCProperties /D /Order"),
    ("fields", "/AcroForm /Fields"),
    ("calculation", "/AcroForm /CO"),
    ("kids", "a form field's /Kids"),
    ("options", "a form field's /Opt"),
];

fn assemble(objects: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut b = Builder::new();
    for (num, body) in objects {
        b.object(*num, body);
    }
    let rows = b.offsets.clone();
    b.finish(52, &rows, "")
}

#[test]
fn reordering_anything_whose_order_has_meaning_is_refused_or_reported() {
    // Order carries meaning in each of these: which annotation is on top and the tab order,
    // the order text is read in, the pages', the outline's, the layers panel's and the fields'
    // navigation and calculation order. Each is compared in order, never as a set: reordered,
    // in a rewrite (with a redaction) and in an appended update, it is refused or reported.
    let base = assemble(&ordered_objects(false, ""));
    let clean = inspect(&base, &base, InspectLimits::default()).unwrap();
    assert_eq!(clean, Inspection { base_kept: true, pages: PageChanges { before: 2, after: 2, ..PageChanges::default() }, ..Inspection::default() });
    let redacted = inspect(&base, &assemble(&ordered_objects(true, "")), InspectLimits::default()).unwrap();
    assert_eq!((redacted.content.pages_changed_outside_redactions, redacted.redaction_areas.len()), (0, 1), "{redacted:?}");
    let caught = |result: &Result<Inspection, InspectError>| match result {
        Err(InspectError::Edited(_)) => true,
        Ok(inspection) => {
            inspection.content.pages_with_content_changed > 0
                || inspection.content.pages_changed_outside_redactions > 0
                || inspection.form_fields_changed > 0
                || inspection.pages.moved > 0
                || inspection.annotations != AnnotationChanges::default()
                || inspection.outlines != OutlineChanges::default()
        }
        Err(_) => false,
    };
    let prev = Some(last_section(&base));
    let plain = ordered_objects(false, "");
    for (swap, what) in ORDERS {
        let rewrite = inspect(&base, &assemble(&ordered_objects(true, swap)), InspectLimits::default());
        assert!(caught(&rewrite), "{what}, in a rewrite: {rewrite:?}");
        let changed: Vec<(u32, Vec<u8>)> = ordered_objects(false, swap).into_iter().filter(|object| !plain.contains(object)).collect();
        assert!(!changed.is_empty(), "{what}");
        let objects: Vec<Written<'_>> = changed.iter().map(|(num, body)| (*num, body.as_slice())).collect();
        let appended = inspect(&base, &update_section(&base, &objects, prev, ""), InspectLimits::default());
        assert!(caught(&appended), "{what}, appended: {appended:?}");
    }
}

// ── R6b-42: the document information ────────────────────────────────────────────────────────

/// A one-page file showing "Alpha" and, unless `boxed`, a word at (150, 20) (with `boxed`, a
/// black box over where it was: a rewrite redacting it), with `info` as object 6 and the
/// trailer entries `trailer`.
fn informed(boxed: bool, info: &[u8], trailer: &str) -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let last = if boxed { "0 g 146 14 16 18 re f" } else { "BT /F1 12 Tf 150 20 Td (X) Tj ET" };
    b.object(4, &stream("", &[text("Alpha"), b"\n".to_vec(), last.as_bytes().to_vec()].concat()));
    b.object(5, b"null");
    b.object(6, info);
    let rows = b.offsets.clone();
    b.finish(7, &rows, trailer)
}

#[test]
fn the_document_information_is_compared_in_both_kinds_of_save() {
    // The reviewer's I-1: PDFium shows the title (and the portal its properties), but neither an
    // appended update nor a rewrite compared `/Info` (aedee8c reported every one of these as
    // unchanged, or as a redaction and nothing else). Each changed key is a fact now, which the
    // host refuses.
    const BASE_INFO: &[u8] = b"<< /Title (Supply Agreement 2027) /Producer (Writer) >>";
    let base = informed(false, BASE_INFO, "/Info 6 0 R");
    let changed = |result: Result<Inspection, InspectError>, case: &str| -> Vec<String> {
        result.unwrap_or_else(|error| panic!("{case}: {error:?}")).info.changed
    };
    assert!(changed(inspect(&base, &base, InspectLimits::default()), "unchanged").is_empty());
    let prev = Some(last_section(&base));
    let form = stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", b"0 g 0 0 10 10 re f");
    // Appended updates: (case, objects written, trailer entries, the keys reported).
    let forged: &[u8] = b"<< /Title (Settlement and Waiver) /Producer (Writer) >>";
    for (case, objects, trailer, keys) in [
        ("another title", vec![(6u32, forged)], "/Info 6 0 R", vec!["Title"]),
        ("a new object with another title", vec![(7, forged)], "/Info 7 0 R", vec!["Title"]),
        ("the newest trailer without /Info", vec![], "", vec!["Producer", "Title"]),
        (
            "a date added",
            vec![(6, b"<< /Title (Supply Agreement 2027) /Producer (Writer) /ModDate (D:20271114) >>".as_slice())],
            "/Info 6 0 R",
            vec!["ModDate"],
        ),
        // A value by reference is compared as such (another encoding of the same title).
        (
            "a title by reference",
            vec![(6, b"<< /Title 7 0 R /Producer (Writer) >>".as_slice()), (7, b"(Supply Agreement 2027)".as_slice())],
            "/Info 6 0 R",
            vec!["Title"],
        ),
        // A key that is not plain ASCII is written as a PDF name writes it.
        (
            "an odd key",
            vec![(6, b"<< /Title (Supply Agreement 2027) /Producer (Writer) /Odd#20Key#E9 (x) >>".as_slice())],
            "/Info 6 0 R",
            vec!["Odd#20Key#E9"],
        ),
    ] {
        let edited = update_section(&base, &objects, prev, trailer);
        let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert!(inspection.base_kept, "{case}");
        assert_eq!(inspection.info.changed, keys, "{case}: {inspection:?}");
    }
    // Readers read anything but a reference to a dictionary differently: refused.
    for (case, objects, trailer) in [
        ("a stream", vec![(6u32, form.as_slice())], "/Info 6 0 R"),
        ("a dictionary in the trailer", vec![], "/Info << /Title (Settlement and Waiver) >>"),
    ] {
        let edited = update_section(&base, &objects, prev, trailer);
        assert!(refused_against(&base, &edited).contains("/Info"), "{case}");
    }
    // Rewrites: a word redacted, and the document information as given.
    for (case, info, trailer, keys) in [
        ("kept", BASE_INFO, "/Info 6 0 R", vec![]),
        ("another title", forged, "/Info 6 0 R", vec!["Title"]),
        ("removed", b"null".as_slice(), "", vec!["Producer", "Title"]),
    ] {
        let inspection = inspect(&base, &informed(true, info, trailer), InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert!(!inspection.base_kept, "{case}");
        assert_eq!(inspection.redaction_areas.len(), 1, "{case}: {inspection:?}");
        assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{case}: {inspection:?}");
        assert_eq!(inspection.info.changed, keys, "{case}: {inspection:?}");
    }
    assert!(refused_against(&base, &informed(true, &form, "/Info 6 0 R")).contains("/Info"), "a rewrite's /Info as a stream");
    // A base whose document information readers read differently is the base's problem.
    let odd = informed(false, b"null", "/Info << /Title (Supply Agreement 2027) >>");
    assert!(matches!(inspect(&odd, &odd, InspectLimits::default()), Err(InspectError::Base(_))));
}

// ── R6b-41: every removal accepted is reported ──────────────────────────────────────────────

/// A one-page tagged file: "Alpha" drawn in a span (`/Span << /MCID 0 {span} >> BDC`) and,
/// unless `boxed`, a word at (150, 20) (with `boxed`, a black box over it: a rewrite redacting
/// it). The page has the entries `page`; the catalog `catalog`, a structure tree (8, with one
/// figure element 9 having the entries `element`) and an XMP packet (6); a thumbnail is
/// object 7.
fn removable(boxed: bool, catalog: &str, page: &str, span: &str, element: &str) -> Vec<u8> {
    let mut b = Builder::new();
    b.object(1, format!("<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 8 0 R /MarkInfo << /Marked true >> {catalog} >>").as_bytes());
    b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} /StructParents 0 {page} >>").as_bytes());
    let last = if boxed { "0 g 146 14 16 18 re f" } else { "BT /F1 12 Tf 150 20 Td (X) Tj ET" };
    let content = format!("/Span << /MCID 0 {span} >> BDC\n{}\nEMC\n{last}", String::from_utf8(text("Alpha")).unwrap());
    b.object(4, &stream("", content.as_bytes()));
    b.object(5, b"null");
    // What the catalog or the page no longer names is null: a rewrite carries nothing nothing
    // reaches (R6b-48).
    let xmp = stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>");
    b.object(6, if catalog.contains("/Metadata") { &xmp } else { b"null" });
    let thumbnail = stream("/Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", b"\xff");
    b.object(7, if page.contains("/Thumb") { &thumbnail } else { b"null" });
    b.object(8, b"<< /Type /StructTreeRoot /K 9 0 R >>");
    b.object(9, format!("<< /S /Figure /P 8 0 R /Pg 3 0 R /K 0 {element} >>").as_bytes());
    let rows = b.offsets.clone();
    b.finish(10, &rows, "")
}

#[test]
fn every_removal_a_rewrite_makes_is_reported() {
    // Follow-up 4 accepted these removals without a fact (aedee8c had no field for them).
    const CATALOG: &str = "/Metadata 6 0 R";
    const PAGE: &str = "/AA << /O << /S /URI /URI (https://example.com/opened) >> >> /Thumb 7 0 R";
    const SPAN: &str = "/ActualText (Alpha)";
    const ELEMENT: &str = "/Alt (A signature)";
    let base = removable(false, CATALOG, PAGE, SPAN, ELEMENT);
    assert_eq!(inspect(&base, &base, InspectLimits::default()).unwrap().structure_removed, StructureRemovals::default());
    let removals = |tagged_text, metadata, actions, thumbnails| StructureRemovals { tagged_text, metadata, actions, thumbnails };
    // (case, catalog, page, span, element, what is reported)
    for (case, catalog, page, span, element, expected) in [
        ("nothing taken away", CATALOG, PAGE, SPAN, ELEMENT, removals(0, 0, 0, 0)),
        ("the element's text", CATALOG, PAGE, SPAN, "", removals(1, 0, 0, 0)),
        ("the span's text", CATALOG, PAGE, "", ELEMENT, removals(1, 0, 0, 0)),
        ("both texts", CATALOG, PAGE, "", "", removals(2, 0, 0, 0)),
        ("the XMP packet", "", PAGE, SPAN, ELEMENT, removals(0, 1, 0, 0)),
        ("the page's actions", CATALOG, "/Thumb 7 0 R", SPAN, ELEMENT, removals(0, 0, 1, 0)),
        ("the page's thumbnail", CATALOG, "/AA << /O << /S /URI /URI (https://example.com/opened) >> >>", SPAN, ELEMENT, removals(0, 0, 0, 1)),
        ("everything", "", "", "", "", removals(2, 1, 1, 1)),
    ] {
        let edited = removable(true, catalog, page, span, element);
        let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(inspection.structure_removed, expected, "{case}: {inspection:?}");
        assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{case}: {inspection:?}");
        assert_eq!(inspection.redaction_areas.len(), 1, "{case}: {inspection:?}");
    }
    // An appended update may take none of these away: a page's entries count as its content
    // changed (and the tags and the XMP packet fail), never as a removal.
    let prev = Some(last_section(&base));
    let page = format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} /StructParents 0 /Thumb 7 0 R >>");
    let inspection = inspect(&base, &update_section(&base, &[(3, page.as_bytes())], prev, ""), InspectLimits::default()).unwrap();
    assert_eq!((inspection.structure_removed, inspection.content.pages_with_content_changed), (StructureRemovals::default(), 1), "{inspection:?}");
}

/// A one-page file drawing two spans, `/Span << {first} >> BDC … EMC` around "Price" and
/// `/Span << {second} >> BDC … EMC` around "Total", and, unless `boxed`, a word at (150, 20)
/// (with `boxed`, a black box over it).
fn two_spans(boxed: bool, first: &str, second: &str) -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    let last = if boxed { "0 g 146 14 16 18 re f" } else { "BT /F1 12 Tf 150 20 Td (X) Tj ET" };
    let content = format!(
        "/Span << {first} >> BDC BT /F1 12 Tf 20 150 Td (Price) Tj ET EMC\n/Span << {second} >> BDC BT /F1 12 Tf 20 100 Td (Total) Tj ET EMC\n{last}"
    );
    b.object(4, &stream("", content.as_bytes()));
    let rows = b.offsets.clone();
    b.finish(5, &rows, "")
}

#[test]
fn property_list_text_is_compared_span_by_span_in_drawing_order() {
    // R6b-41: aedee8c compared a page's property-list text as one multiset, so text swapped
    // between spans, or moved from one span to another, passed a rewrite as a redaction.
    let base = two_spans(false, "/ActualText (Price 100)", "/ActualText (Price 900)");
    // (case, first span, second span, accepted, text entries taken away)
    for (case, first, second, accepted, taken) in [
        ("as it was", "/ActualText (Price 100)", "/ActualText (Price 900)", true, 0),
        ("one taken away", "", "/ActualText (Price 900)", true, 1),
        ("swapped", "/ActualText (Price 900)", "/ActualText (Price 100)", false, 0),
        ("one changed", "/ActualText (Price 100)", "/ActualText (Price 1)", false, 0),
        ("one added", "/ActualText (Price 100) /Alt (Price)", "/ActualText (Price 900)", false, 0),
    ] {
        let inspection =
            inspect(&base, &two_spans(true, first, second), InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        let outside = u32::from(!accepted);
        assert_eq!(inspection.content.pages_changed_outside_redactions, outside, "{case}: {inspection:?}");
        if accepted {
            assert_eq!(inspection.structure_removed.tagged_text, taken, "{case}: {inspection:?}");
        }
    }
    // Text moved from the first span to the second (one only: the multiset is the same).
    let one = two_spans(false, "/ActualText (Price 100)", "");
    let moved = inspect(&one, &two_spans(true, "", "/ActualText (Price 100)"), InspectLimits::default()).unwrap();
    assert_eq!(moved.content.pages_changed_outside_redactions, 1, "{moved:?}");
}

// ── R6b-44: the structure tree's text, element by element ───────────────────────────────────

/// A tagged one-page file as [`tagged`] makes it (a rewrite with `boxed`), whose structure tree
/// (object 6) has the kids `kids` and the elements `elements` (objects 7 onwards).
fn tree(boxed: bool, kids: &str, elements: &[String]) -> Vec<u8> {
    let mut objects = vec![format!("<< /Type /StructTreeRoot /K {kids} >>").into_bytes()];
    objects.extend(elements.iter().map(|element| element.clone().into_bytes()));
    tagged(boxed, "/StructTreeRoot 6 0 R /MarkInfo << /Marked true >>", &objects.iter().map(Vec::as_slice).collect::<Vec<_>>())
}

#[test]
fn structure_tree_text_is_compared_element_by_element() {
    // The reviewer's I-3: aedee8c compared the tags' text as one multiset for the document, so
    // a rewrite with a word redaction could swap it between elements or reorder like elements.
    let element = |role: &str, parent: u32, text: &str, kids: &str| format!("<< /S /{role} /P {parent} 0 R /Pg 3 0 R {text} /K {kids} >>");
    let figures = |first: &str, second: &str| vec![element("Figure", 6, first, "0"), element("Figure", 6, second, "1")];
    let seller = "/Alt (Signature of the Seller)";
    let buyer = "/Alt (Signature of the Buyer)";
    let base = tree(false, "[7 0 R 8 0 R]", &figures(seller, buyer));
    let spans = |first: &str, second: &str| vec![element("Span", 6, first, "0"), element("Span", 6, second, "1")];
    let priced = tree(false, "[7 0 R 8 0 R]", &spans("/ActualText (Price 100)", "/ActualText (Price 900)"));
    let plain = tree(false, "[7 0 R 8 0 R]", &figures("", ""));
    let named = tree(false, "[7 0 R 8 0 R]", &figures("/Alt /Seller", ""));
    // (case, base, rewrite, text entries taken away; `None`: refused)
    type Case<'a> = (&'a str, &'a Vec<u8>, Vec<u8>, Option<u32>);
    let cases: Vec<Case<'_>> = vec![
        ("as it was", &base, tree(true, "[7 0 R 8 0 R]", &figures(seller, buyer)), Some(0)),
        ("one alternate text taken away", &base, tree(true, "[7 0 R 8 0 R]", &figures("", buyer)), Some(1)),
        (
            "a marked-content kid taken away",
            &base,
            tree(true, "[7 0 R 8 0 R]", &[element("Figure", 6, seller, "[]"), element("Figure", 6, buyer, "1")]),
            Some(0),
        ),
        ("/Alt swapped between the figures", &base, tree(true, "[7 0 R 8 0 R]", &figures(buyer, seller)), None),
        (
            "/ActualText swapped between the spans",
            &priced,
            tree(true, "[7 0 R 8 0 R]", &spans("/ActualText (Price 900)", "/ActualText (Price 100)")),
            None,
        ),
        ("the figures reordered in the root's /K", &base, tree(true, "[8 0 R 7 0 R]", &figures(seller, buyer)), None),
        (
            "figures without text, their content swapped",
            &plain,
            tree(true, "[7 0 R 8 0 R]", &[element("Figure", 6, "", "1"), element("Figure", 6, "", "0")]),
            None,
        ),
        (
            "the second figure moved under the first",
            &plain,
            tree(true, "[7 0 R]", &[element("Figure", 6, "", "[0 8 0 R]"), element("Figure", 7, "", "1")]),
            None,
        ),
        ("an alternate text that is not a string changed", &named, tree(true, "[7 0 R 8 0 R]", &figures("/Alt /Buyer", "")), None),
    ];
    for (case, base, edited, taken) in cases {
        let result = inspect(base, &edited, InspectLimits::default());
        match taken {
            Some(taken) => {
                let inspection = result.unwrap_or_else(|error| panic!("{case}: {error:?}"));
                assert_eq!(inspection.structure_removed.tagged_text, taken, "{case}: {inspection:?}");
                assert_eq!(inspection.redaction_areas.len(), 1, "{case}: {inspection:?}");
            }
            None => match result {
                Err(InspectError::Edited(message)) => assert!(message.contains("structure tree"), "{case}: {message}"),
                other => panic!("{case}: {other:?}"),
            },
        }
    }
}

#[test]
fn a_session_redaction_reports_the_tag_text_it_took_away() {
    // The redactor cleans the tags of what it removed: the element whose marked content it
    // emptied loses its alternate and actual text, and that marked-content kid. Accepted as a
    // redaction, the text taken away reported (R6b-41, R6b-44); the other element keeps its own.
    use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};
    let mut b = Builder::new();
    b.object(1, b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 5 0 R /MarkInfo << /Marked true >> >>");
    b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} /StructParents 0 >>").as_bytes());
    let content = b"/P << /MCID 0 >> BDC BT /F1 12 Tf 20 200 Td (SECRET) Tj ET EMC\n/P << /MCID 1 >> BDC BT /F1 12 Tf 20 100 Td (Public) Tj ET EMC";
    b.object(4, &stream("", content));
    b.object(5, b"<< /Type /StructTreeRoot /K [6 0 R 7 0 R] >>");
    b.object(6, b"<< /S /P /P 5 0 R /Pg 3 0 R /ActualText (SECRET) /Alt (the secret) /K 0 >>");
    b.object(7, b"<< /S /P /P 5 0 R /Pg 3 0 R /ActualText (Public) /K 1 >>");
    let rows = b.offsets.clone();
    let base = b.finish(8, &rows, "");
    let mark = Shape::Redact { quads: vec![rect_quad([16.0, 190.0, 120.0, 220.0])], overlay: String::new(), look: Default::default() };
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    let id = session.open("base.pdf", None, std::sync::Arc::new(base.clone()), None).unwrap();
    session
        .apply(
            id,
            Edit::AddAnnotation(NewAnnotation {
                page: 0,
                style: Style::default_for(&mark),
                shape: mark,
                contents: String::new(),
                author: "Sari".into(),
            }),
        )
        .unwrap();
    session.apply(id, Edit::ApplyRedactions { pages: None }).unwrap();
    let redacted = session.save_bytes(id).unwrap();
    let inspection = inspect(&base, &redacted, InspectLimits::default()).unwrap();
    assert!(!inspection.base_kept, "{inspection:?}");
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!(inspection.structure_removed, StructureRemovals { tagged_text: 2, ..StructureRemovals::default() }, "{inspection:?}");
}

// ── R6b-47: text standing for removed glyphs goes with them ─────────────────────────────────

/// The text standing for "SECRET" in the base of [`alternate_text`].
const SECRET_SPAN: &str = "/ActualText (SECRET) /Alt (the secret) /E (SECRET)";

/// A one-page tagged file drawing "SECRET" at (20, 200) in `/Span << /MCID 0 {first} >>` (with
/// `named`, a list named `/S0` in the resources) and "Public" at (20, 100) in
/// `/Span << /MCID 1 /ActualText (Public) >>`, both in an outer level `/Div << {outer} >>`. The
/// structure tree: a section (`{section}`) holding a paragraph for each MCID, the first with
/// `{element}`, the second `/ActualText (Public)`. With `redacted`, "SECRET" is gone and a black
/// box lies over its place; `tail` is drawn last.
fn alternate_text(redacted: bool, named: bool, outer: &str, first: &str, section: &str, element: &str, tail: &str) -> Vec<u8> {
    alternate_text_with(redacted, named, outer, first, section, element, "/ActualText (Public)", tail)
}

/// [`alternate_text`], the text of "Public" (its span's and its element's) `public`.
#[allow(clippy::too_many_arguments)]
fn alternate_text_with(redacted: bool, named: bool, outer: &str, first: &str, section: &str, element: &str, public: &str, tail: &str) -> Vec<u8> {
    let mut b = Builder::new();
    b.object(1, b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 5 0 R /MarkInfo << /Marked true >> >>");
    b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
    let (list, properties) = if named {
        ("/S0".to_string(), format!("/Properties << /S0 << /MCID 0 {first} >> >>"))
    } else {
        (format!("<< /MCID 0 {first} >>"), String::new())
    };
    b.object(
        3,
        format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> {properties} >> /StructParents 0 >>")
            .as_bytes(),
    );
    let (secret, boxed) = if redacted { ("", "0 g 16 190 70 26 re f\n") } else { ("(SECRET) Tj", "") };
    let content = format!(
        "/Div << {outer} >> BDC\n/Span {list} BDC BT /F1 12 Tf 20 200 Td {secret} ET EMC\n/Span << /MCID 1 {public} >> BDC BT /F1 12 Tf 20 100 Td (Public) Tj ET EMC\nEMC\n{boxed}{tail}"
    );
    b.object(4, &stream("", content.as_bytes()));
    b.object(5, b"<< /Type /StructTreeRoot /K 6 0 R >>");
    b.object(6, format!("<< /S /Sect /P 5 0 R /Pg 3 0 R /K [7 0 R 8 0 R] {section} >>").as_bytes());
    b.object(7, format!("<< /S /P /P 6 0 R /K 0 {element} >>").as_bytes());
    b.object(8, format!("<< /S /P /P 6 0 R /K 1 {public} >>").as_bytes());
    let rows = b.offsets.clone();
    b.finish(9, &rows, "")
}

#[test]
fn a_rewrite_keeping_text_that_stands_for_removed_glyphs_is_outside_the_redactions() {
    // R6b-47: 1d93b91 accepted a redaction that kept an inline property list's /ActualText
    // around the removed glyphs (follow-up 5, concern 5): the redacted words survived for
    // copying and assistive technology.
    let outside = |base: &[u8], edited: &[u8]| inspect(base, edited, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions;
    let section = "/Alt (A section about the SECRET)";
    let base = alternate_text(false, false, "", SECRET_SPAN, section, "/ActualText (SECRET)", "");
    // Everything standing for "SECRET" taken away: a redaction, the text taken away reported.
    let clean = alternate_text(true, false, "", "", "", "", "");
    let inspection = inspect(&base, &clean, InspectLimits::default()).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.structure_removed.tagged_text), (0, 5), "{inspection:?}");
    assert_eq!(inspection.text.pages_with_text_removed, 1, "{inspection:?}");
    // Any of it kept: the span's, the element's, or an ancestor's.
    for (case, first, section, element) in [
        ("the span's /ActualText", "/ActualText (SECRET)", "", ""),
        ("the span's /Alt", "/Alt (the secret)", "", ""),
        ("the span's /E", "/E (SECRET)", "", ""),
        ("the element's /ActualText", "", "", "/ActualText (SECRET)"),
        ("the section's /Alt", "", section, ""),
    ] {
        assert_eq!(outside(&base, &alternate_text(true, false, "", first, section, element, "")), 1, "{case}");
    }
    // /Lang and /T do not stand for the content: kept, they are no leak.
    let base = alternate_text(false, false, "", "/ActualText (SECRET) /Lang (en)", "", "/T (Heading)", "");
    assert_eq!(outside(&base, &alternate_text(true, false, "", "/Lang (en)", "", "/T (Heading)", "")), 0);
    // A list named in the resources, and a level around the span.
    let base = alternate_text(false, true, "", SECRET_SPAN, "", "", "");
    assert_eq!(outside(&base, &alternate_text(true, true, "", "/ActualText (SECRET)", "", "", "")), 1, "named, kept");
    assert_eq!(outside(&base, &alternate_text(true, true, "", "", "", "", "")), 0, "named, taken away");
    let base = alternate_text(false, false, "/ActualText (SECRET Public)", "", "", "", "");
    assert_eq!(outside(&base, &alternate_text(true, false, "/ActualText (SECRET Public)", "", "", "", "")), 1, "outer level, kept");
    assert_eq!(outside(&base, &alternate_text(true, false, "", "", "", "", "")), 0, "outer level, taken away");
    // Text standing for glyphs that stay is kept: "Public" keeps its /ActualText throughout.
}

#[test]
fn where_the_readers_disagree_on_marked_content_no_such_text_may_stay() {
    // An `EMC` closing what its stream did not open: which glyph is in which span is each
    // reader's guess, so any text standing for content left on the page's spans, or in the
    // structure tree, counts outside the redactions once glyphs were removed.
    let base = alternate_text(false, false, "", "/ActualText (SECRET)", "", "", "EMC\n");
    let edited = |public: &str| alternate_text_with(true, false, "", "", "", "", public, "EMC\n");
    let outside = |edited: &[u8]| inspect(&base, edited, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions;
    assert_eq!(outside(&edited("/ActualText (Public)")), 1);
    assert_eq!(outside(&edited("")), 0);
}

/// A one-page file drawing form `/Fm1` once, whose content is "SECRET" at (20, 200) in
/// `/Span << {span} >>` and "Public" at (20, 100); with `redacted`, "SECRET" is gone and a black
/// box lies over its place.
fn form_with_span(redacted: bool, span: &str) -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /XObject << /Fm1 5 0 R >> >> >>");
    let boxed = if redacted { "\n0 g 16 190 70 26 re f" } else { "" };
    b.object(4, &stream("", format!("/Fm1 Do{boxed}").as_bytes()));
    let secret = if redacted { "" } else { "(SECRET) Tj" };
    let form = format!("/Span << {span} >> BDC BT /F1 12 Tf 20 200 Td {secret} ET EMC BT /F1 12 Tf 20 100 Td (Public) Tj ET");
    b.object(5, &stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 200 300] {RESOURCES}"), form.as_bytes()));
    let rows = b.offsets.clone();
    b.finish(6, &rows, "")
}

#[test]
fn a_span_in_a_form_loses_its_text_with_its_glyphs() {
    let base = form_with_span(false, "/ActualText (SECRET)");
    let outside = |edited: &[u8]| inspect(&base, edited, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions;
    assert_eq!(outside(&form_with_span(true, "/ActualText (SECRET)")), 1);
    assert_eq!(outside(&form_with_span(true, "")), 0);
}

/// Every text `bytes` exposes, lower-cased: the glyphs hayro extracts from each page, and every
/// string of every object in the file (decoded stream data included: a property list written
/// in a content stream is there).
fn exposed_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    let hayro = pdfcraft_render::content::ContentDocument::open(std::sync::Arc::new(bytes.to_vec()), u64::MAX).unwrap();
    for page in 0..hayro.page_count() {
        out.extend(hayro.content(page).unwrap().glyphs.chars);
        out.push('\n');
    }
    fn walk(object: &pdfcraft_cos::Object, out: &mut String) {
        match object {
            pdfcraft_cos::Object::String(string) => out.push_str(&String::from_utf8_lossy(&string.bytes)),
            pdfcraft_cos::Object::Array(items) => items.iter().for_each(|item| walk(item, out)),
            pdfcraft_cos::Object::Dict(dict) => dict.iter().for_each(|(_, value)| walk(value, out)),
            pdfcraft_cos::Object::Stream(stream) => {
                stream.dict.iter().for_each(|(_, value)| walk(value, out));
                out.push_str(&String::from_utf8_lossy(&stream.decoded().unwrap()));
            }
            _ => {}
        }
        out.push('\n');
    }
    let doc = pdfcraft_cos::Document::open(std::sync::Arc::new(bytes.to_vec())).unwrap();
    let size = doc.trailer().int(b"Size").unwrap();
    for num in 1..u32::try_from(size).unwrap() {
        if let Ok(object) = doc.try_get(num) {
            walk(&object, &mut out);
        }
    }
    out.to_lowercase()
}

#[test]
fn a_session_redaction_takes_away_the_text_standing_for_what_it_removed() {
    // R6b-47, the redactor's side: a real redaction of "SECRET" on a tagged page whose spans,
    // their enclosing level, the paragraph and its section all carry text standing for it is
    // accepted, and no text the file exposes (extracted, /ActualText, /Alt, /E) says it any more;
    // "Public" keeps its own.
    use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};
    for named in [false, true] {
        let base = alternate_text(
            false,
            named,
            "/ActualText (SECRET Public) /E (the SECRET and the Public)",
            SECRET_SPAN,
            "/Alt (A section about the SECRET)",
            "/ActualText (SECRET) /Alt (the secret) /E (SECRET)",
            "",
        );
        assert!(exposed_text(&base).contains("secret"));
        let mark = Shape::Redact { quads: vec![rect_quad([16.0, 190.0, 120.0, 220.0])], overlay: String::new(), look: Default::default() };
        let mut session = Session::new().with_clock(|| 1_700_000_000);
        session.set_keep_document_information(true);
        let id = session.open("base.pdf", None, std::sync::Arc::new(base.clone()), None).unwrap();
        let note = NewAnnotation { page: 0, style: Style::default_for(&mark), shape: mark, contents: String::new(), author: "Sari".into() };
        session.apply(id, Edit::AddAnnotation(note)).unwrap();
        session.apply(id, Edit::ApplyRedactions { pages: None }).unwrap();
        let redacted = session.save_bytes(id).unwrap();
        let inspection = inspect(&base, &redacted, InspectLimits::default()).unwrap();
        assert!(!inspection.base_kept, "named {named}: {inspection:?}");
        assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "named {named}: {inspection:?}");
        assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "named {named}: {inspection:?}");
        let exposed = exposed_text(&redacted);
        let leaks: Vec<&str> = exposed.lines().filter(|line| line.contains("secret")).collect();
        assert!(leaks.is_empty(), "named {named}: {leaks:?}");
        assert!(exposed.contains("public"), "named {named}");
    }
}

#[test]
fn a_session_redaction_on_unbalanced_marked_content_takes_every_span_s_text_away() {
    // Follow-up 6 audit: an `EMC` closing nothing makes which glyph is in which span each
    // reader's guess. The redactor takes every span's text away on that page (and its elements'),
    // so the inspection accepts.
    use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};
    let mut b = Builder::new();
    b.object(1, b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 7 0 R /MarkInfo << /Marked true >> >>");
    b.object(2, b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 200 300] >>");
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} /StructParents 0 >>").as_bytes());
    let content = b"/Span << /ActualText (SECRET) >> BDC BT /F1 12 Tf 20 200 Td (SECRET) Tj ET EMC EMC\n/P << /MCID 0 /ActualText (Public) >> BDC BT /F1 12 Tf 20 100 Td (Public) Tj ET EMC";
    b.object(4, &stream("", content));
    b.object(5, format!("<< /Type /Page /Parent 2 0 R /Contents 6 0 R {RESOURCES} /StructParents 1 >>").as_bytes());
    b.object(6, &stream("", b"/P << /MCID 0 >> BDC BT /F1 12 Tf 20 200 Td (Other) Tj ET EMC"));
    b.object(7, b"<< /Type /StructTreeRoot /K [8 0 R 9 0 R] >>");
    b.object(8, b"<< /S /P /P 7 0 R /Pg 3 0 R /K 0 /ActualText (Public) >>");
    b.object(9, b"<< /S /P /P 7 0 R /Pg 5 0 R /K 0 /ActualText (Other) >>");
    let rows = b.offsets.clone();
    let base = b.finish(10, &rows, "");
    let mark = Shape::Redact { quads: vec![rect_quad([16.0, 190.0, 120.0, 220.0])], overlay: String::new(), look: Default::default() };
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    let id = session.open("base.pdf", None, std::sync::Arc::new(base.clone()), None).unwrap();
    let note = NewAnnotation { page: 0, style: Style::default_for(&mark), shape: mark, contents: String::new(), author: "Sari".into() };
    session.apply(id, Edit::AddAnnotation(note)).unwrap();
    session.apply(id, Edit::ApplyRedactions { pages: None }).unwrap();
    let redacted = session.save_bytes(id).unwrap();
    let inspection = inspect(&base, &redacted, InspectLimits::default()).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    let exposed = exposed_text(&redacted);
    assert!(!exposed.contains("secret"), "{exposed}");
    assert!(exposed.contains("other"), "the other page keeps its word");
}

// ── follow-up 6 audit: where the two readers could read different objects ─────────────────

/// A page drawing "Alpha" with a square annotation (5) and document information (6).
fn probe_base() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [5 0 R] {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, b"<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] >>");
    b.object(6, b"<< /Title (Base) >>");
    let rows = b.offsets.clone();
    b.finish(7, &rows, "/Info 6 0 R")
}

/// `base` with an update appended: `objects` (number, generation, body) defined, `free` (number,
/// generation) listed free, one subsection each, and the trailer entries `trailer` (`/Prev` added).
fn probe_update(base: &[u8], objects: &[(u32, u16, Vec<u8>)], free: &[(u32, u16)], trailer: &str) -> Vec<u8> {
    let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
    let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let mut out = base.to_vec();
    let mut rows = Vec::new();
    for (num, generation, body) in objects {
        rows.push((*num, format!("{:010} {generation:05} n \n", out.len())));
        out.extend_from_slice(format!("{num} {generation} obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    for (num, generation) in free {
        rows.push((*num, format!("0000000000 {generation:05} f \n")));
    }
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (num, row) in rows {
        out.extend_from_slice(format!("{num} 1\n{row}").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< {trailer} /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

#[test]
fn objects_the_readers_could_take_differently_fail_closed() {
    // Each case either reads alike in both readers (and is compared) or fails the inspection.
    let base = probe_base();
    let refused = |case: &str, edited: &[u8], why: &str| match inspect(&base, edited, InspectLimits::default()) {
        Err(InspectError::Edited(message)) => assert!(message.contains(why), "{case}: {message}"),
        other => panic!("{case}: expected Err(Edited), got {other:?}"),
    };
    let info =
        |case: &str, edited: &[u8]| inspect(&base, edited, InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}")).info.changed;
    refused("free /Info in update", &probe_update(&base, &[], &[(6, 1)], "/Size 7 /Root 1 0 R /Info 6 0 R"), "readers disagree");
    refused("free annot in update", &probe_update(&base, &[], &[(5, 1)], "/Size 7 /Root 1 0 R /Info 6 0 R"), "readers disagree");
    refused("free content in update", &probe_update(&base, &[], &[(4, 1)], "/Size 7 /Root 1 0 R /Info 6 0 R"), "readers disagree");
    refused(
        "info gen 1 redefined",
        &probe_update(&base, &[(6, 1, b"<< /Title (Forged) >>".to_vec())], &[], "/Size 7 /Root 1 0 R /Info 6 0 R"),
        "another generation",
    );
    refused(
        "content gen 1 redefined",
        &probe_update(&base, &[(4, 1, stream("", &text("Bravo")))], &[], "/Size 7 /Root 1 0 R /Info 6 0 R"),
        "another generation",
    );
    refused("info beyond size", &probe_update(&base, &[(9, 0, b"<< /Title (Nine) >>".to_vec())], &[], "/Size 7 /Root 1 0 R /Info 9 0 R"), "/Size");
    refused(
        "content beyond size",
        &probe_update(
            &base,
            &[
                (9, 0, stream("", &text("Bravo"))),
                (3, 0, format!("<< /Type /Page /Parent 2 0 R /Contents 9 0 R /Annots [5 0 R] {RESOURCES} >>").into_bytes()),
            ],
            &[],
            "/Size 7 /Root 1 0 R /Info 6 0 R",
        ),
        "/Size",
    );
    refused("info ref gen 1 to gen 0 object", &probe_update(&base, &[], &[], "/Size 7 /Root 1 0 R /Info 6 1 R"), "another generation");
    refused("size shrunk", &probe_update(&base, &[], &[], "/Size 5 /Root 1 0 R /Info 6 0 R"), "/Size");
    refused("no root in update trailer", &probe_update(&base, &[], &[], "/Size 7 /Info 6 0 R"), "damaged");
    // An update whose table is hybrid: its /XRefStm stream lists /Info 6 at a second definition
    // ("Stream"), the table at "Table".
    {
        let mut out = base.clone();
        let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
        let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        let table_def = out.len();
        out.extend_from_slice(b"6 0 obj\n<< /Title (Table) >>\nendobj\n");
        let stream_def = out.len();
        out.extend_from_slice(b"6 0 obj\n<< /Title (Stream) >>\nendobj\n");
        let at9 = out.len();
        let mut entries = vec![1u8];
        entries.extend_from_slice(&u32::try_from(stream_def).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0, 1]);
        entries.extend_from_slice(&u32::try_from(at9).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0]);
        out.extend_from_slice(b"9 0 obj\n");
        out.extend_from_slice(&stream(&format!("/Type /XRef /Size 10 /W [1 4 2] /Index [6 1 9 1] /Root 1 0 R /Prev {prev}"), &entries));
        out.extend_from_slice(b"\nendobj\n");
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \n6 1\n{table_def:010} 00000 n \ntrailer\n<< /Size 10 /Root 1 0 R /Info 6 0 R /Prev {prev} /XRefStm {at9} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        // hayro rebuilds a hybrid update's cross-reference data: refused.
        refused("hybrid update, both list 6", &out, "damaged");
        // The table leaves 6 out, the stream lists it.
        let mut out2 = base.clone();
        let stream_def = out2.len();
        out2.extend_from_slice(b"6 0 obj\n<< /Title (Stream) >>\nendobj\n");
        let at9 = out2.len();
        let mut entries = vec![1u8];
        entries.extend_from_slice(&u32::try_from(stream_def).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0, 1]);
        entries.extend_from_slice(&u32::try_from(at9).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0]);
        out2.extend_from_slice(b"9 0 obj\n");
        out2.extend_from_slice(&stream(&format!("/Type /XRef /Size 10 /W [1 4 2] /Index [6 1 9 1] /Root 1 0 R /Prev {prev}"), &entries));
        out2.extend_from_slice(b"\nendobj\n");
        let xref = out2.len();
        out2.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 10 /Root 1 0 R /Info 6 0 R /Prev {prev} /XRefStm {at9} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        refused("hybrid update, stream alone lists 6", &out2, "damaged");
        // A pure cross-reference stream update after a table base.
        let mut out3 = base.clone();
        let def = out3.len();
        out3.extend_from_slice(b"6 0 obj\n<< /Title (XRef) >>\nendobj\n");
        let at9 = out3.len();
        let mut entries = vec![1u8];
        entries.extend_from_slice(&u32::try_from(def).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0, 1]);
        entries.extend_from_slice(&u32::try_from(at9).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0]);
        out3.extend_from_slice(b"9 0 obj\n");
        out3.extend_from_slice(&stream(&format!("/Type /XRef /Size 10 /W [1 4 2] /Index [6 1 9 1] /Root 1 0 R /Info 6 0 R /Prev {prev}"), &entries));
        out3.extend_from_slice(b"\nendobj\n");
        out3.extend_from_slice(format!("startxref\n{at9}\n%%EOF\n").as_bytes());
        assert_eq!(info("xref stream update", &out3), ["Title"]);
    }
    // An object stream whose header names object 6 twice.
    {
        let mut out = base.clone();
        let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
        let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        let first = "<< /Title (First) >>";
        let second = "<< /Title (Second) >>";
        let header = format!("6 0 6 {} ", first.len() + 1);
        let data = format!("{header}{first} {second}");
        let at8 = out.len();
        out.extend_from_slice(b"8 0 obj\n");
        out.extend_from_slice(&stream(&format!("/Type /ObjStm /N 2 /First {}", header.len()), data.as_bytes()));
        out.extend_from_slice(b"\nendobj\n");
        let at9 = out.len();
        let mut entries = vec![2u8, 0, 0, 0, 8, 0, 1, 1];
        entries.extend_from_slice(&u32::try_from(at8).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0, 1]);
        entries.extend_from_slice(&u32::try_from(at9).unwrap().to_be_bytes());
        entries.extend_from_slice(&[0, 0]);
        out.extend_from_slice(b"9 0 obj\n");
        out.extend_from_slice(&stream(&format!("/Type /XRef /Size 10 /W [1 4 2] /Index [6 1 8 2] /Root 1 0 R /Info 6 0 R /Prev {prev}"), &entries));
        out.extend_from_slice(b"\nendobj\n");
        out.extend_from_slice(format!("startxref\n{at9}\n%%EOF\n").as_bytes());
        // Both readers (as PDFium and pdf.js) take the object at the index the cross-reference
        // data names, not the first header entry naming its number.
        assert_eq!(info("object stream naming 6 twice", &out), ["Title"]);
    }
    // A body after the last startxref nobody lists.
    let mut tail = base.clone();
    tail.extend_from_slice(b"6 0 obj\n<< /Title (Tail) >>\nendobj\n");
    assert!(info("unlisted tail", &tail).is_empty(), "read by neither");
}

// ── R6b-48: a rewrite carries nothing nothing reaches ───────────────────────────────────────

/// [`tagged`]'s boxed rewrite written with a cross-reference stream, its page and content in the
/// file and, in an object stream, the catalog, the page tree and `extra` (objects 6…), each
/// listed.
fn rewrite_with_object_stream(extra: &[&str]) -> Vec<u8> {
    let mut b = Builder::new();
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &[text("Alpha"), b"\n0 g 146 14 16 18 re f".to_vec()].concat()));
    let mut members: Vec<(u32, String)> =
        vec![(1, "<< /Type /Catalog /Pages 2 0 R >>".into()), (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".into())];
    members.extend(extra.iter().enumerate().map(|(i, body)| (6 + i as u32, (*body).to_string())));
    let mut header = String::new();
    let mut bodies = String::new();
    for (num, body) in &members {
        header.push_str(&format!("{num} {} ", bodies.len()));
        bodies.push_str(body);
        bodies.push(' ');
    }
    let container = 6 + extra.len() as u32;
    b.object(container, &stream(&format!("/Type /ObjStm /N {} /First {}", members.len(), header.len()), format!("{header}{bodies}").as_bytes()));
    let xref_num = container + 1;
    let at = b.out.len();
    let row = |kind: u8, field: usize, extra: u16| {
        let mut out = vec![kind];
        out.extend_from_slice(&u32::try_from(field).unwrap().to_be_bytes());
        out.extend_from_slice(&extra.to_be_bytes());
        out
    };
    let mut entries = row(0, 0, 65535);
    for num in 1..xref_num {
        entries.extend(match members.iter().position(|(member, _)| *member == num) {
            Some(index) => row(2, container as usize, index as u16),
            None if num == 5 => row(0, 0, 0),
            None => row(1, b.offsets[&num], 0),
        });
    }
    entries.extend(row(1, at, 0));
    b.object(xref_num, &stream(&format!("/Type /XRef /Size {} /W [1 4 2] /Root 1 0 R", xref_num + 1), &entries));
    b.out.extend_from_slice(format!("startxref\n{at}\n%%EOF\n").as_bytes());
    b.out
}

#[test]
fn a_rewrite_carrying_what_nothing_reaches_is_refused() {
    // R6b-48: 1d93b91 accepted a redaction whose file still held the unredacted original, as an
    // object nothing reaches: no viewer shows it, any tool reading the file finds it.
    let base = tagged(false, "", &[]);
    let original = stream("", b"BT /F1 12 Tf 150 20 Td (X) Tj ET");
    assert_eq!(inspect(&base, &tagged(true, "", &[]), InspectLimits::default()).unwrap().content.pages_changed_outside_redactions, 0);
    let message = match inspect(&base, &tagged(true, "", &[&original]), InspectLimits::default()) {
        Err(InspectError::Edited(message)) => message,
        other => panic!("an orphan object: {other:?}"),
    };
    assert!(message.contains("holds object 6, which nothing reaches"), "{message}");
    // In an object stream: the same, and its object stream reached only through what it holds.
    let clean = rewrite_with_object_stream(&[]);
    assert_eq!(inspect(&base, &clean, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions, 0);
    let message = match inspect(&base, &rewrite_with_object_stream(&["<< /Original (BT /F1 12 Tf 150 20 Td (X) Tj ET) >>"]), InspectLimits::default())
    {
        Err(InspectError::Edited(message)) => message,
        other => panic!("an orphan in an object stream: {other:?}"),
    };
    assert!(message.contains("holds object 6, which nothing reaches"), "{message}");
    // An earlier section's objects, superseded: carried too.
    let page = format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>");
    let boxed = tagged(true, "", &[]);
    let prev = Some(last_section(&boxed));
    let message = match inspect(&base, &update_section(&boxed, &[(3, page.as_bytes())], prev, ""), InspectLimits::default()) {
        Err(InspectError::Edited(message)) => message,
        other => panic!("two sections: {other:?}"),
    };
    assert!(message.contains("cross-reference sections"), "{message}");
}

#[test]
fn the_session_s_redaction_reaches_everything_it_writes() {
    use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};
    let base = tagged(false, "", &[]);
    let mark = Shape::Redact { quads: vec![rect_quad([140.0, 10.0, 170.0, 40.0])], overlay: String::new(), look: Default::default() };
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    let id = session.open("base.pdf", None, std::sync::Arc::new(base.clone()), None).unwrap();
    let note = NewAnnotation { page: 0, style: Style::default_for(&mark), shape: mark, contents: String::new(), author: "Sari".into() };
    session.apply(id, Edit::AddAnnotation(note)).unwrap();
    session.apply(id, Edit::ApplyRedactions { pages: None }).unwrap();
    let inspection = inspect(&base, &session.save_bytes(id).unwrap(), InspectLimits::default()).unwrap();
    assert!(!inspection.base_kept, "{inspection:?}");
    assert_eq!(inspection.text.pages_with_text_removed, 1, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
}

// ── follow-up 7 audit: nothing hides what a rewrite took away ────────────────────────────────

/// What [`hiding`] writes beside a tagged page.
#[derive(Default)]
struct Hiding<'a> {
    catalog: &'a str,
    page: &'a str,
    /// The paragraph's own entries.
    element: &'a str,
    /// Objects 7… as given.
    extra: Vec<(u32, Vec<u8>)>,
    /// Objects written but listed free.
    free: Vec<(u32, Vec<u8>)>,
    /// Bytes written before the page tree (object 2).
    comment: &'a [u8],
    /// Appended after `%%EOF`.
    tail: &'a [u8],
    /// Trailer entries.
    trailer: &'a str,
    /// The structure tree root's `/ParentTree` (default: a direct number tree).
    parent_tree: &'a str,
}

/// A tagged one-page file: "Alpha" in marked content 0 (the paragraph, object 6), and "X" at
/// (150, 20) or, with `boxed`, a black box there; with what `hiding` adds.
fn hiding(boxed: bool, hiding: &Hiding<'_>) -> Vec<u8> {
    let mut b = Builder::new();
    b.object(1, format!("<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 5 0 R /MarkInfo << /Marked true >> {} >>", hiding.catalog).as_bytes());
    b.out.extend_from_slice(hiding.comment);
    b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} /StructParents 0 {} >>", hiding.page).as_bytes());
    let last = if boxed { "0 g 146 14 16 18 re f" } else { "BT /F1 12 Tf 150 20 Td (X) Tj ET" };
    let content = format!("/P << /MCID 0 >> BDC\n{}\nEMC\n{last}", String::from_utf8(text("Alpha")).unwrap());
    b.object(4, &stream("", content.as_bytes()));
    let parent_tree = if hiding.parent_tree.is_empty() { "<< /Nums [0 [6 0 R]] >>" } else { hiding.parent_tree };
    b.object(5, format!("<< /Type /StructTreeRoot /K 6 0 R /ParentTree {parent_tree} >>").as_bytes());
    b.object(6, format!("<< /S /P /P 5 0 R /Pg 3 0 R /K 0 {} >>", hiding.element).as_bytes());
    for (num, body) in &hiding.extra {
        b.object(*num, body);
    }
    let mut rows = b.offsets.clone();
    for (num, body) in &hiding.free {
        b.object(*num, body);
        rows.remove(num);
    }
    let size = rows.keys().chain(hiding.free.iter().map(|(num, _)| num)).max().copied().unwrap_or(6) + 1;
    let mut out = b.finish(size, &rows, hiding.trailer);
    out.extend_from_slice(hiding.tail);
    out
}

#[test]
fn a_rewrite_cannot_hide_what_it_took_away() {
    // Each route the follow-up 7 audit tried for the unredacted "X" to stay in a rewrite:
    // refused, or reported where the host refuses it.
    let base = hiding(false, &Hiding::default());
    let edited = |what: Hiding<'_>| hiding(true, &what);
    let inspection = inspect(&base, &edited(Hiding::default()), InspectLimits::default()).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.text.pages_with_text_removed), (0, 1), "{inspection:?}");
    let secret = || stream("", b"BT /F1 12 Tf 150 20 Td (X) Tj ET");
    let refused_for = |case: &str, file: &[u8], why: &str| match inspect(&base, file, InspectLimits::default()) {
        Err(InspectError::Edited(message)) => assert!(message.contains(why), "{case}: {message}"),
        other => panic!("{case}: expected Err(Edited), got {other:?}"),
    };
    let outside = |case: &str, file: &[u8]| {
        let inspection = inspect(&base, file, InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{case}: {inspection:?}");
    };
    // Bytes no cross-reference data lists: a definition listed free (the free-entry chain), one
    // after `%%EOF`, a comment between objects.
    let lists = "bytes no cross-reference data lists";
    refused_for("listed free", &edited(Hiding { free: vec![(7, secret())], ..Hiding::default() }), lists);
    refused_for("after %%EOF", &edited(Hiding { tail: b"7 0 obj\n(X)\nendobj\n", ..Hiding::default() }), lists);
    refused_for("a comment", &edited(Hiding { comment: b"% BT /F1 12 Tf 150 20 Td (X) Tj ET\n", ..Hiding::default() }), lists);
    // Reached only through a link no comparison reads (an element's /Obj).
    refused_for(
        "through a link",
        &edited(Hiding { element: "/Obj 7 0 R", extra: vec![(7, b"<< /X (X) >>".to_vec())], ..Hiding::default() }),
        "no comparison reads",
    );
    // The structure tree's parent tree is compared, each element by its place.
    let indexed = Hiding { parent_tree: "7 0 R", extra: vec![(7, b"<< /Nums [0 [6 0 R]] /X 8 0 R >>".to_vec()), (8, secret())], ..Hiding::default() };
    refused_for("in the parent tree", &edited(indexed), "structure tree");
    let kept = Hiding { parent_tree: "7 0 R", extra: vec![(7, b"<< /Nums [0 [6 0 R]] >>".to_vec())], ..Hiding::default() };
    assert!(inspect(&hiding(false, &kept), &edited(kept), InspectLimits::default()).is_ok(), "a parent tree kept, as an object");
    // A page's entries no viewer draws: kept or removed only.
    outside("page /PieceInfo, written in the page", &edited(Hiding { page: "/PieceInfo << /App << /Private (X) >> >>", ..Hiding::default() }));
    outside(
        "page /PieceInfo, an object",
        &edited(Hiding { page: "/PieceInfo << /App << /Private 7 0 R >> >>", extra: vec![(7, secret())], ..Hiding::default() }),
    );
    outside("page /Metadata", &edited(Hiding { page: "/Metadata 7 0 R", extra: vec![(7, secret())], ..Hiding::default() }));
    // The catalog's entries are compared whole: /PieceInfo, /AF, embedded files.
    for (case, catalog) in [
        ("catalog /PieceInfo", "/PieceInfo << /App << /Private 7 0 R >> >>"),
        ("catalog /AF", "/AF [7 0 R]"),
        ("embedded file", "/Names << /EmbeddedFiles << /Names [(x) 7 0 R] >> >>"),
    ] {
        refused_for(case, &edited(Hiding { catalog, extra: vec![(7, secret())], ..Hiding::default() }), "changed");
    }
    // Links written as a value in place of a reference are compared: an element's /Obj or /P.
    refused_for("an element's /Obj", &edited(Hiding { element: "/Obj << /X (X) >>", ..Hiding::default() }), "structure tree");
    // The document information: reported (the host refuses any change, R6b-42).
    let informed = edited(Hiding { trailer: "/Info 7 0 R", extra: vec![(7, b"<< /Title (X) >>".to_vec())], ..Hiding::default() });
    assert_eq!(inspect(&base, &informed, InspectLimits::default()).unwrap().info.changed, ["Title"]);
}

#[test]
fn a_back_reference_written_as_a_value_is_compared() {
    // An annotation's /P (its page) is left out of digests as a back reference; written as a
    // string it carried text no fact compared, in an appended update as in a rewrite.
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [5 0 R] {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, b"<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /P 3 0 R >>");
    let rows = b.offsets.clone();
    let base = b.finish(6, &rows, "");
    let prev = Some(last_section(&base));
    let edited = update_section(&base, &[(5, b"<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /P (X) >>")], prev, "");
    let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!(inspection.annotations.changed, 1, "{inspection:?}");
}

#[test]
fn the_reachability_walk_fails_closed() {
    // Follow-up 7 audit, item 5. An object only the walk reaches that cannot be read is refused,
    // not passed over; and a walk past the objects bound is too complex, not accepted.
    let base = hiding(false, &Hiding::default());
    let unreadable = hiding(true, &Hiding { element: "/Obj 7 0 R", extra: vec![(7, b"<< /Unterminated (".to_vec())], ..Hiding::default() });
    assert!(matches!(inspect(&base, &unreadable, InspectLimits::default()), Err(InspectError::Edited(_))));
    // A chain of 2,000 arrays, each the next one's only reference, under an element's /Obj.
    let chain: Vec<(u32, Vec<u8>)> =
        (7..2_007u32).map(|num| (num, if num < 2_006 { format!("[{} 0 R]", num + 1) } else { "[]".into() }.into_bytes())).collect();
    let long = hiding(true, &Hiding { element: "/Obj 7 0 R", extra: chain, ..Hiding::default() });
    assert!(inspect(&base, &long, InspectLimits::default()).is_ok(), "within the bound");
    let limits = Limits { objects: 500, ..Limits::default() };
    assert_eq!(inspect_with(&base, &long, limits), Err(InspectError::TooComplex));
}

/// A one-page file: object 3 the page (`page` its extra entries, `resources` its resources,
/// with `/F1` Helvetica unless they name their own `/Font`), drawing "Alpha" and `last`; object
/// 5 `extra`.
fn laid_out(page: &str, resources: &str, last: &str, extra: &[u8]) -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    let fonts = if resources.contains("/Font") { "" } else { "/Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >>" };
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << {fonts} {resources} >> {page} >>").as_bytes());
    b.object(4, &stream("", &[text("Alpha"), b"\n".to_vec(), last.as_bytes().to_vec()].concat()));
    b.object(5, if extra.is_empty() { b"null" } else { extra });
    let rows = b.offsets.clone();
    b.finish(6, &rows, "")
}

const X: &str = "BT /F1 12 Tf 150 20 Td (X) Tj ET";
const X_BOX: &str = "0 g 146 14 16 18 re f";

#[test]
fn a_resource_nothing_draws_cannot_keep_what_a_rewrite_took_away() {
    // Follow-up 7 audit, item 7: the unredacted "X" kept as a form nothing draws, under the
    // page's resources, passed (the resources were compared only through what was drawn).
    let form = stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300]", X.as_bytes());
    let base = laid_out("", "", X, b"");
    let outside = |edited: &[u8]| inspect(&base, edited, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions;
    assert_eq!(outside(&laid_out("", "", X_BOX, b"")), 0);
    assert_eq!(outside(&laid_out("", "/XObject << /Hidden 5 0 R >>", X_BOX, &form)), 1, "a new entry nothing draws");
    // The base drawing X as a form: kept, undrawn, under its name, after the redaction.
    let base = laid_out("", "/XObject << /Fm 5 0 R >>", "/Fm Do", &form);
    let outside = |edited: &[u8]| inspect(&base, edited, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions;
    assert_eq!(outside(&laid_out("", "/XObject << /Fm 5 0 R >>", X_BOX, &form)), 1, "the base's drawn form left undrawn");
    assert_eq!(outside(&laid_out("", "", X_BOX, b"")), 0, "the form gone with what it drew");
    // An entry the base did not draw either may stay; the editor's standard fonts may be added.
    let base = laid_out("", "/XObject << /Unused 5 0 R >>", X, &form);
    let outside = |edited: &[u8]| inspect(&base, edited, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions;
    assert_eq!(outside(&laid_out("", "/XObject << /Unused 5 0 R >>", X_BOX, &form)), 0);
    let fonts = |added: &str| format!("/Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> {added} >>");
    let base = laid_out("", "", X, b"");
    let outside = |edited: &[u8]| inspect(&base, edited, InspectLimits::default()).unwrap().content.pages_changed_outside_redactions;
    let standard = fonts("/PCHelv << /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>");
    assert_eq!(outside(&laid_out("", &standard, X_BOX, b"")), 0, "the editor's standard font");
    let other = fonts("/F9 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 5 0 R >>");
    assert_eq!(outside(&laid_out("", &other, X_BOX, &stream("", X.as_bytes()))), 1, "a font holding more");
}

#[test]
fn a_page_tree_viewers_count_otherwise_is_refused() {
    // Follow-up 7 audit, item 6: PDFium takes the root's /Count; a /Count that lies, or an entry
    // with /Kids that is not a /Pages node, shows another document there.
    let base = laid_out("", "", X, b"");
    let tree = |pages: &str, second: &str| {
        let mut b = Builder::new();
        b.object(1, b"<< /Type /Catalog /Pages 2 0 R >>");
        b.object(2, format!("<< /Type /Pages {pages} /MediaBox [0 0 200 300] >>").as_bytes());
        b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
        b.object(4, &stream("", &[text("Alpha"), b"\n".to_vec(), X.as_bytes().to_vec()].concat()));
        b.object(5, second.as_bytes());
        b.object(6, format!("<< /Type /Page /Parent 5 0 R /Contents 4 0 R {RESOURCES} >>").as_bytes());
        let rows = b.offsets.clone();
        b.finish(7, &rows, "")
    };
    let pages = |edited: &[u8]| inspect(&base, edited, InspectLimits::default());
    assert_eq!(pages(&tree("/Kids [3 0 R 5 0 R] /Count 2", "<< /Type /Pages /Parent 2 0 R /Kids [6 0 R] /Count 1 >>")).unwrap().pages.inserted, 1);
    for (case, file) in [
        ("the root's /Count", tree("/Kids [3 0 R 5 0 R] /Count 1", "<< /Type /Pages /Parent 2 0 R /Kids [6 0 R] /Count 1 >>")),
        ("a node's /Count", tree("/Kids [3 0 R 5 0 R] /Count 2", "<< /Type /Pages /Parent 2 0 R /Kids [6 0 R] /Count 3 >>")),
        ("no /Count", tree("/Kids [3 0 R 5 0 R] /Count 2", "<< /Type /Pages /Parent 2 0 R /Kids [6 0 R] >>")),
        ("/Kids on a page", tree("/Kids [3 0 R 5 0 R] /Count 2", "<< /Type /Page /Parent 2 0 R /Kids [6 0 R] /Count 1 >>")),
        ("/Kids without a type", tree("/Kids [3 0 R 5 0 R] /Count 2", "<< /Parent 2 0 R /Kids [6 0 R] /Count 1 >>")),
    ] {
        match pages(&file) {
            Err(InspectError::Edited(message)) => assert!(message.contains("page tree"), "{case}: {message}"),
            other => panic!("{case}: {other:?}"),
        }
    }
}

#[test]
fn the_page_s_boxes_rotation_and_unit_are_compared() {
    // Follow-up 7 audit, item 6 (pinned, already so): a box that hides what a rewrite kept, the
    // unit, the media box: outside the redactions; a rotation: reported; a crop box past the
    // media box (viewers show their intersection): unchanged.
    let base = laid_out("", "", X, b"");
    let inspection = |page: &str| inspect(&base, &laid_out(page, "", X, b""), InspectLimits::default()).unwrap();
    for page in ["/CropBox [0 50 200 300]", "/UserUnit 2", "/MediaBox [0 50 200 300]"] {
        assert_eq!(inspection(page).content.pages_changed_outside_redactions, 1, "{page}");
    }
    for page in ["/Rotate 90", "/Rotate -90"] {
        let found = inspection(page);
        assert_eq!((found.pages.rotated, found.content.pages_changed_outside_redactions), (1, 0), "{page}");
    }
    assert_eq!(inspection("/CropBox [-100 -100 400 400]").content.pages_changed_outside_redactions, 0);
}

// ── follow-up 7 audit, item 10: a redaction is applied only where nothing survives under it ──

const MARK: &str = "<< /Type /Annot /Subtype /Redact /Rect [146 14 162 32] /QuadPoints [146 32 162 32 146 14 162 14] /IC [0 0 0] >>";
/// An image placed over the mark's area.
const IMAGE_THERE: &str = "q 16 0 0 18 146 14 cm /Im Do Q";

/// A page drawing "Alpha" and `last`, with `annots` (objects 10…) and the image `/Im` (object 6).
fn marked_page(last: &str, annots: &[&str]) -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    let list: Vec<String> = (0..annots.len()).map(|i| format!("{} 0 R", 10 + i)).collect();
    b.object(
        3,
        format!(
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [{}] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> /XObject << /Im 6 0 R >> >> >>",
            list.join(" ")
        )
        .as_bytes(),
    );
    b.object(4, &stream("", &[text("Alpha"), b"\n".to_vec(), last.as_bytes().to_vec()].concat()));
    b.object(5, b"null");
    b.object(6, &stream("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8", &[0, 80, 160, 240]));
    for (i, annot) in annots.iter().enumerate() {
        b.object(10 + i as u32, annot.as_bytes());
    }
    for num in 7..10 {
        b.object(num, b"null");
    }
    let rows = b.offsets.clone();
    b.finish(10 + annots.len() as u32, &rows, "")
}

#[test]
fn a_box_over_content_that_survives_is_no_redaction() {
    // Forged rewrites: the mark gone, a black box drawn over its area, and what was under it
    // kept (still copied, searched, read aloud or shown in a comment). 206f2ce counted each
    // redaction applied, the page inside its redactions.
    let note = "<< /Type /Annot /Subtype /FreeText /Rect [148 16 160 30] /Contents (X) /DA (/Helv 12 Tf 0 g) >>";
    for (case, base, edited, expected) in [
        ("the text removed", marked_page(X, &[MARK]), marked_page(X_BOX, &[]), (0, 1)),
        ("the text kept under the box", marked_page(X, &[MARK]), marked_page(&format!("{X}\n{X_BOX}"), &[]), (1, 0)),
        ("an image kept under the box", marked_page(IMAGE_THERE, &[MARK]), marked_page(&format!("{IMAGE_THERE}\n{X_BOX}"), &[]), (1, 0)),
        ("a comment kept over the box", marked_page(X, &[MARK, note]), marked_page(X_BOX, &[note]), (1, 0)),
    ] {
        let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(
            (inspection.content.pages_changed_outside_redactions, inspection.annotations.redactions_applied),
            expected,
            "{case}: {inspection:?}"
        );
    }
}

#[test]
fn text_standing_for_a_removed_annotation_goes_with_it() {
    // Follow-up 7 audit, item 10: a link under the mark removed, its element's /Alt (naming it
    // through /OBJR) kept. 206f2ce accepted it.
    let link = "<< /Type /Annot /Subtype /Link /Rect [148 16 160 30] /A << /S /URI /URI (https://x.example) >> >>";
    let tagged_page = |last: &str, annots: &[&str], alt: &str| {
        let mut b = Builder::new();
        b.object(1, b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 7 0 R /MarkInfo << /Marked true >> >>");
        b.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>");
        let list: Vec<String> = (0..annots.len()).map(|i| format!("{} 0 R", 10 + i)).collect();
        b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [{}] {RESOURCES} >>", list.join(" ")).as_bytes());
        b.object(4, &stream("", &[text("Alpha"), b"\n".to_vec(), last.as_bytes().to_vec()].concat()));
        for num in [5, 6, 9] {
            b.object(num, b"null");
        }
        b.object(7, b"<< /Type /StructTreeRoot /K 8 0 R >>");
        let kid = if annots.len() > 1 { "/K << /Type /OBJR /Obj 11 0 R >>" } else { "" };
        b.object(8, format!("<< /S /Link /P 7 0 R /Pg 3 0 R {alt} {kid} >>").as_bytes());
        for (i, annot) in annots.iter().enumerate() {
            b.object(10 + i as u32, annot.as_bytes());
        }
        let rows = b.offsets.clone();
        b.finish(10 + annots.len() as u32, &rows, "")
    };
    let base = tagged_page(X, &[MARK, link], "/Alt (https://x.example)");
    let result = |alt: &str| inspect(&base, &tagged_page(X_BOX, &[], alt), InspectLimits::default());
    assert_eq!(result("").unwrap().content.pages_changed_outside_redactions, 0);
    match result("/Alt (https://x.example)") {
        Err(InspectError::Edited(message)) => assert!(message.contains("annotation the rewrite took away"), "{message}"),
        other => panic!("{other:?}"),
    }
    // The Session applying the mark: the link, its element's text and reference go; accepted.
    let mut session = crate::Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    let id = session.open("base.pdf", None, std::sync::Arc::new(base.clone()), None).unwrap();
    session.apply(id, crate::Edit::ApplyRedactions { pages: None }).unwrap();
    let redacted = session.save_bytes(id).unwrap();
    let inspection = inspect(&base, &redacted, InspectLimits::default()).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.annotations.redactions_applied), (0, 1), "{inspection:?}");
    assert!(!exposed_text(&redacted).contains("x.example"));
}

#[test]
fn text_added_outside_the_boxes_is_no_redaction() {
    // Follow-up 7 audit, item 11: the redacted word removed under the box but written again
    // elsewhere (invisible, or white on white) was counted applied, the page inside its
    // redactions (text added was only reported).
    let base = marked_page(X, &[MARK]);
    let facts = |edited: &[u8]| {
        let inspection = inspect(&base, edited, InspectLimits::default()).unwrap();
        (inspection.content.pages_changed_outside_redactions, inspection.annotations.redactions_applied)
    };
    assert_eq!(facts(&marked_page(X_BOX, &[])), (0, 1));
    for again in ["BT 3 Tr /F1 12 Tf 20 250 Td (X) Tj ET", "BT 1 g /F1 12 Tf 20 250 Td (X) Tj ET"] {
        assert_eq!(facts(&marked_page(&format!("{X_BOX}\n{again}"), &[])), (1, 0), "{again}");
    }
    // (A mark's overlay label is text added too: such a page was already outside its
    // redactions, the label breaking the box's uniform fill.)
}

// ── follow-up 8 (R6b-51): the page tree and the structure tree's pages ──────────────────────

/// Two pages under node 2 (which gives page 1 its box, rotation and Helvetica): page 3 draws
/// "Alpha" and names `parent` its parent; page 4 has its own box, rotation and Courier. `node`
/// adds entries to node 2.
fn two_under_one(parent: &str, node: &str) -> Vec<u8> {
    let mut b = Builder::new();
    b.object(1, b"<< /Type /Catalog /Pages 2 0 R >>");
    b.object(2, format!("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 200 300] /Rotate 0 {RESOURCES} {node} >>").as_bytes());
    b.object(3, format!("<< /Type /Page /Parent {parent} /Contents 5 0 R >>").as_bytes());
    b.object(
        4,
        b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Rotate 90 /MediaBox [0 0 400 100] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Courier >> >> >> >>",
    );
    b.object(5, &stream("", &text("Alpha")));
    b.object(6, &stream("", b"BT /F1 12 Tf 20 50 Td (Bravo) Tj ET"));
    let rows = b.offsets.clone();
    b.finish(7, &rows, "")
}

#[test]
fn a_page_whose_parent_is_not_the_node_listing_it_is_refused() {
    // Re-review C1: PDFium and pdf.js inherit through a page's own /Parent; a page naming
    // another page as its parent showed page 1 rotated, at 400 × 100, in Courier there, and
    // was accepted with every fact zero.
    let base = two_under_one("2 0 R", "");
    assert_eq!(inspect(&base, &base, InspectLimits::default()).unwrap().pages.rotated, 0);
    let update = update_section(&base, &[(3, b"<< /Type /Page /Parent 4 0 R /Contents 5 0 R >>")], Some(last_section(&base)), "");
    for (case, edited) in [("an appended update", update), ("a rewrite", two_under_one("4 0 R", ""))] {
        let message = refused_against(&base, &edited);
        assert!(message.contains("/Parent"), "{case}: {message}");
    }
    // An intermediate node naming another parent, and a root naming one.
    let nested = |inner_parent: &str, root: &str| {
        let mut b = Builder::new();
        b.object(1, b"<< /Type /Catalog /Pages 2 0 R >>");
        b.object(2, format!("<< /Type /Pages /Kids [7 0 R] /Count 1 /MediaBox [0 0 200 300] {RESOURCES} {root} >>").as_bytes());
        b.object(7, format!("<< /Type /Pages /Parent {inner_parent} /Kids [3 0 R] /Count 1 >>").as_bytes());
        b.object(3, b"<< /Type /Page /Parent 7 0 R /Contents 5 0 R >>");
        b.object(5, &stream("", &text("Alpha")));
        b.object(6, b"<< /Type /Pages /Kids [] /Count 0 /Rotate 90 >>");
        let rows = b.offsets.clone();
        b.finish(8, &rows, "")
    };
    let base = nested("2 0 R", "");
    assert!(inspect(&base, &base, InspectLimits::default()).is_ok());
    for (case, edited) in [("an intermediate node's", nested("6 0 R", "")), ("the root's", nested("2 0 R", "/Parent 6 0 R"))] {
        let message = refused_against(&base, &edited);
        assert!(message.contains("/Parent"), "{case}: {message}");
    }
    // The base itself read otherwise by viewers: no fact about it holds.
    match inspect(&two_under_one("4 0 R", ""), &two_under_one("4 0 R", ""), InspectLimits::default()) {
        Err(InspectError::Base(message)) => assert!(message.contains("/Parent"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_page_tree_node_s_own_entries_are_compared() {
    // Re-review M5: a node's entries other than its kids, count, parent, type and inherited
    // attributes were compared in neither kind of save: /Secret (SECRET X) was accepted.
    let base = two_under_one("2 0 R", "/Extra (kept)");
    assert!(inspect(&base, &base, InspectLimits::default()).is_ok());
    let node =
        format!("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 200 300] /Rotate 0 {RESOURCES} /Extra (kept) /Secret (SECRET X) >>");
    let update = update_section(&base, &[(2, node.as_bytes())], Some(last_section(&base)), "");
    for (case, edited) in [
        ("added in an update", update),
        ("added in a rewrite", two_under_one("2 0 R", "/Extra (kept) /Secret (SECRET X)")),
        ("changed in a rewrite", two_under_one("2 0 R", "/Extra (SECRET X)")),
        ("removed in a rewrite", two_under_one("2 0 R", "")),
    ] {
        let message = refused_against(&base, &edited);
        assert!(message.contains("page tree node"), "{case}: {message}");
    }
}

/// Two tagged pages: element 7 (`/Pg` `pg7`) holds page 1's "Alpha" (MCID 0), element 8 page
/// 2's "Bravo" (MCID 0).
fn tagged_pages(pg7: &str) -> Vec<u8> {
    let mut b = Builder::new();
    b.object(1, b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R /MarkInfo << /Marked true >> >>");
    b.object(2, format!("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 200 300] {RESOURCES} >>").as_bytes());
    b.object(3, b"<< /Type /Page /Parent 2 0 R /Contents 9 0 R /StructParents 0 >>");
    b.object(4, b"<< /Type /Page /Parent 2 0 R /Contents 10 0 R /StructParents 1 >>");
    b.object(5, b"null");
    b.object(6, b"<< /Type /StructTreeRoot /K [7 0 R 8 0 R] /ParentTree << /Nums [0 [7 0 R] 1 [8 0 R]] >> >>");
    b.object(7, format!("<< /S /P /P 6 0 R /Pg {pg7} /K 0 >>").as_bytes());
    b.object(8, b"<< /S /P /P 6 0 R /Pg 4 0 R /K 0 >>");
    b.object(9, &stream("", b"/P << /MCID 0 >> BDC BT /F1 12 Tf 20 150 Td (Alpha) Tj ET EMC"));
    b.object(10, &stream("", b"/P << /MCID 0 >> BDC BT /F1 12 Tf 20 150 Td (Bravo) Tj ET EMC"));
    let rows = b.offsets.clone();
    b.finish(11, &rows, "")
}

#[test]
fn marked_content_stays_on_its_page() {
    // Re-review I2: an element's /Pg was left out of every comparison, so its marked content
    // could be moved to another page (the tags then read "Bravo" twice and "Alpha" nowhere).
    let base = tagged_pages("3 0 R");
    assert!(inspect(&base, &base, InspectLimits::default()).is_ok());
    let update = update_section(&base, &[(7, b"<< /S /P /P 6 0 R /Pg 4 0 R /K 0 >>")], Some(last_section(&base)), "");
    for (case, edited) in [("an appended update", update), ("a rewrite", tagged_pages("4 0 R"))] {
        let message = refused_against(&base, &edited);
        assert!(message.contains("/Pg"), "{case}: {message}");
    }
    // Pages moved by the Session: the kids follow their pages.
    let mut session = crate::Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    let id = session.open("base.pdf", None, std::sync::Arc::new(base.clone()), None).unwrap();
    session.apply(id, crate::Edit::MovePages { pages: vec![1], to: 0 }).unwrap();
    let moved = session.save_bytes(id).unwrap();
    let inspection = inspect(&base, &moved, InspectLimits::default()).unwrap();
    assert_eq!(inspection.pages.moved, 1, "{inspection:?}");
}
