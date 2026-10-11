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

/// Like [`length_short_of_endstream`], with the short stream an annotation's appearance, which
/// hayro does not read (annotations are compared, not drawn).
fn appearance_short_of_endstream() -> Vec<u8> {
    let mut b = Builder::new();
    head(&mut b);
    b.object(3, format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [5 0 R] {RESOURCES} >>").as_bytes());
    b.object(4, &stream("", &text("Alpha")));
    b.object(5, b"<< /Type /Annot /Subtype /Square /Rect [20 200 80 260] /AP << /N 6 0 R >> >>");
    let mut body = b"<< /Type /XObject /Subtype /Form /BBox [0 0 60 60] /Length 10 >>\nstream\n".to_vec();
    body.extend_from_slice(b"0 0 1 RG 2 2 56 56 re S");
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
    // reading is checked to be the same bytes. Tolerated (as the files signed by some tools have
    // it), and compared as usual: the page draws what the base's does.
    let inspection = inspect(&sound(), &length_short_of_endstream(), InspectLimits::default()).unwrap();
    assert_eq!(inspection.text, TextChanges::default(), "{inspection:?}");
    assert_eq!(inspection.content, ContentChanges::default(), "{inspection:?}");
    assert!(inspect(&length_short_of_endstream(), &length_short_of_endstream(), InspectLimits::default()).is_ok());
    // A stream hayro does not read (an annotation's appearance) has no second reading to agree
    // with: its data is what each reader makes of it.
    let message = refused(&appearance_short_of_endstream());
    assert!(message.contains("stream 6 0 R does not have the length it declares"), "{message}");
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
