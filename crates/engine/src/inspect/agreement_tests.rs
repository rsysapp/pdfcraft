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
    for (case, file) in [
        ("unlisted second definition", one_number_twice()),
        ("hybrid, table first", stale_offset_beside_an_object_stream()),
        ("hybrid, free in table", stale_page_free_in_table()),
    ] {
        assert!(inspect(&sound(), &file, InspectLimits::default()).is_ok(), "{case}");
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
    out.extend_from_slice(format!("trailer\n<< /Size 9 /Root 1 0 R {prev} {extra} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
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
        let result = inspect(&base, &file(true, catalog, title), InspectLimits::default());
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
