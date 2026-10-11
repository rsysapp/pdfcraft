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
        match inspect(&base, &edited, InspectLimits::default()) {
            Err(InspectError::Edited(message)) => assert!(message.contains("nothing this comparison reports reads"), "{case}: {message}"),
            other => panic!("{case}: {other:?}"),
        }
    }
    // The document information, which the editor restamps on every save: accepted.
    let edited = update_section(&base, &[(6, b"<< /Title (Agreement) /ModDate (D:20231114221320Z) >>")], prev, "/Info 6 0 R");
    let inspection = inspect(&base, &edited, InspectLimits::default()).unwrap();
    assert_eq!((inspection.text, inspection.content), (TextChanges::default(), ContentChanges::default()), "{inspection:?}");
}
