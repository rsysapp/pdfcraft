use std::sync::Arc;

use pdfcraft_annot::{Meta, NewAnnotation, Shape, Style, add_annotation, rect_quad};
use pdfcraft_cos::{Document, SaveOptions, write_full, write_incremental};

use super::*;
use crate::interp::MAX_NESTING;

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\nendstream");
    v
}

fn pdf(objs: Vec<Vec<u8>>) -> Document {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let x = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offs {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
    Document::open(Arc::new(out)).unwrap()
}

/// A font where every glyph is 500 units wide (so a 10 pt glyph advances 5 pt).
const FONT: &str = "<< /Type /Font /Subtype /TrueType /BaseFont /Arial /FirstChar 32 /LastChar 126 /Widths 95 0 R /FontDescriptor << /Ascent 800 /Descent -200 >> >>";

fn widths() -> Vec<u8> {
    format!("[{}]", vec!["500"; 95].join(" ")).into_bytes()
}

/// One 300×300 page with `content`, font /F1, and extra resources/objects.
fn one_page(content: &[u8], extra_res: &str, extra: Vec<Vec<u8>>) -> Document {
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> {extra_res} >> >>")
            .into_bytes(),
        stream("", content),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
    ];
    objs.extend(extra);
    pdf(objs)
}

fn mark(doc: &mut Document, page: usize, rects: &[[f64; 4]], overlay: &str) {
    let shape = Shape::Redact { quads: rects.iter().map(|r| rect_quad(*r)).collect(), overlay: overlay.into(), look: Default::default() };
    let style = Style::default_for(&shape);
    add_annotation(doc, &NewAnnotation { page, shape, style, contents: String::new(), author: "Tester".into() }, &Meta::default()).unwrap();
}

/// The decoded content streams of a page, joined.
fn content(doc: &Document, page: usize) -> String {
    let p = &pdfcraft_model::pages(doc)[page];
    let (_, data) = page_streams(doc, &p.dict, page).unwrap();
    data.iter().map(|d| String::from_utf8_lossy(d).into_owned()).collect::<Vec<_>>().join("\n")
}

/// Glyphs (and inline images) of page `page` under `rects` (the verifier's count).
fn under(doc: &mut Document, page: usize, rects: &[[f64; 4]]) -> usize {
    let p = pdfcraft_model::pages(doc).swap_remove(page);
    let (_, data) = page_streams(doc, &p.dict, page).unwrap();
    let res = p.dict.get(b"Resources").and_then(|r| doc.resolve(r).as_dict().cloned()).unwrap_or_default();
    let mut rep = Report::default();
    let mut scope = Scope::new(rects, Mode::Verify, &mut rep);
    process(doc, &mut scope, &data, &res, pdfcraft_content::Matrix::IDENTITY).residue
}

fn reopen(doc: &Document) -> Document {
    Document::open(Arc::new(write_incremental(doc, &SaveOptions::default()).unwrap())).unwrap()
}

#[test]
fn glyphs_under_a_mark_go_and_the_rest_stays_put() {
    // "AB1234CD" from x = 10 at 10 pt: each glyph 5 pt wide; 1234 spans x 20–40.
    let mut doc = one_page(b"BT /F1 10 Tf 10 100 Td (AB1234CD) Tj ET", "", vec![]);
    assert_eq!(under(&mut doc, 0, &[[40.0, 95.0, 50.0, 110.0]]), 2, "C and D before");
    mark(&mut doc, 0, &[[20.0, 95.0, 40.0, 110.0]], "");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!((r.marks, r.glyphs), (1, 4));
    let c = content(&doc, 0);
    assert!(c.contains("[(AB) -2000 (CD)] TJ"), "{c}");
    assert!(!c.contains("1234"));
    assert_eq!(under(&mut doc, 0, &[[40.0, 95.0, 50.0, 110.0]]), 2, "C and D are where they were");
    assert_eq!(under(&mut doc, 0, &[[20.0, 95.0, 40.0, 110.0]]), 0);
    // The mark became a black box drawn into the page; the annotation is gone.
    assert!(marks(&doc).is_empty());
    assert!(c.contains("0 0 0 rg") && c.contains("20 95 20 15 re"), "{c}");
    let doc = reopen(&doc);
    assert!(!content(&doc, 0).contains("1234"));
}

#[test]
fn added_text_parameters_do_not_keep_redacted_text() {
    // An Edit ▸ Add content item: its stream dictionary keeps the source text under /PCAdded.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents [4 0 R] /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
        stream("/PCMark /Added /PCAdded << /Kind /Text /Text (AB1234CD) >>", b"BT /F1 10 Tf 10 100 Td (AB1234CD) Tj ET"),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
    ]);
    mark(&mut doc, 0, &[[20.0, 95.0, 40.0, 110.0]], "");
    apply(&mut doc, None).unwrap();
    assert!(!content(&doc, 0).contains("1234"));
    let bytes = write_full(&doc, &SaveOptions::default()).unwrap();
    assert!(!bytes.windows(4).any(|w| w == b"1234"), "redacted text left in the saved file");
}

#[test]
fn operators_split_across_content_streams_are_redacted_whole() {
    // One content stream in three pieces, split between tokens: a marked-content dictionary
    // ends in the second piece, and the TJ array that ends it shows its glyphs with the
    // operator in the third.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents [4 0 R 7 0 R 8 0 R] /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
        stream("", b"/P << /MCID 0"),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        stream("", b">> BDC BT /F1 10 Tf 10 100 Td [(AB1234CD)]"),
        stream("", b"TJ ET EMC BT /F1 10 Tf 10 200 Td (KEEP) Tj ET"),
    ]);
    assert_eq!(under(&mut doc, 0, &[[20.0, 95.0, 40.0, 110.0]]), 4, "the verifier sees the split TJ");
    mark(&mut doc, 0, &[[20.0, 95.0, 40.0, 110.0]], "");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!(r.glyphs, 4, "{r:?}");
    let c = content(&doc, 0);
    assert!(!c.contains("1234"), "redacted glyphs left in the page: {c}");
    assert!(c.contains("KEEP"), "{c}");
    assert_eq!(under(&mut doc, 0, &[[40.0, 95.0, 50.0, 110.0]]), 2, "C and D are where they were");
    // Every operator still has its operands: none was cut off from them.
    for op in pdfcraft_content::parse(c.as_bytes()).ops {
        let want = match op.op.as_slice() {
            b"TJ" | b"Tj" => 1,
            b"BDC" | b"Tf" | b"Td" => 2,
            _ => continue,
        };
        assert_eq!(op.operands.len(), want, "{} lost its operands in {c}", String::from_utf8_lossy(&op.op));
    }
    let doc = reopen(&doc);
    assert!(!content(&doc, 0).contains("1234"));
}

#[test]
fn kerning_spacing_scaling_and_line_operators_are_honoured() {
    // TJ kerning, character and word spacing, 50% horizontal scaling, ' and ".
    let src = b"BT /F1 10 Tf 2 Tc 4 Tw 50 Tz 12 TL 0 200 Td [(AB) -1000 (C D)] TJ (EF) ' 1 0 (GH) \" ET";
    let mut doc = one_page(src, "", vec![]);
    // Line 1 (y 200): A at 0, B at 3.5 (advance (5+2)×0.5), kern +5 → C at 12, space at 15.5,
    // D at 21 (space advance (5+2+4)×0.5 = 5.5). Remove C only.
    mark(&mut doc, 0, &[[12.2, 195.0, 14.8, 210.0]], "");
    // Line 2 (y 188) "EF" and line 3 (y 176) "GH" (Tw 1, Tc 0): remove F and G.
    // (Clear of line 3's glyph boxes, which reach up to 184: any glyph a mark reaches goes.)
    mark(&mut doc, 0, &[[3.8, 185.0, 6.0, 198.0 - 10.0], [0.2, 171.0, 2.3, 180.0]], "");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!(r.glyphs, 3, "{r:?}");
    let c = content(&doc, 0);
    assert!(c.contains("[(AB) -1700 ( D)] TJ"), "{c}");
    assert!(c.contains("T*\n[(E) -700] TJ"), "{c}");
    assert!(c.contains("1 Tw\n0 Tc\nT*\n[-500 (H)] TJ"), "{c}");
    // Everything else stays readable where it was.
    assert_eq!(under(&mut doc, 0, &[[20.0, 195.0, 30.0, 210.0]]), 1, "D");
    assert_eq!(under(&mut doc, 0, &[[2.6, 171.0, 6.0, 180.0]]), 1, "H");
}

#[test]
fn rotated_text_and_composite_fonts() {
    // A Type0 Identity-H font (two-byte codes) with /W widths, and a 90° text matrix.
    let t0 = b"<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /Identity-H /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /BaseFont /X /DW 1000 /W [1 [500 500] 3 4 250] >>] >>".to_vec();
    let src = b"BT /F2 10 Tf 0 1 -1 0 100 50 Tm <0001000200030004> Tj ET";
    let mut doc = one_page(src, "", vec![]);
    let font_ref = doc.add(Object::Dict(Dict::new()));
    let parsed = {
        let mut lx = pdfcraft_cos::Lexer::new(&t0, 0);
        lx.object().unwrap()
    };
    doc.set(font_ref, parsed);
    let page = pdfcraft_model::pages(&doc)[0].obj;
    doc.update_dict(page, |d| {
        let mut res = d.get(b"Resources").and_then(Object::as_dict).cloned().unwrap();
        let mut fonts = res.get(b"Font").and_then(Object::as_dict).cloned().unwrap();
        fonts.set(b"F2".to_vec(), Object::Ref(font_ref));
        res.set(b"Font".to_vec(), Object::Dict(fonts));
        d.set(b"Resources".to_vec(), Object::Dict(res));
    })
    .unwrap();
    // Glyphs run upwards from y 50: CID1 50–55, CID2 55–60, CID3 60–62.5, CID4 62.5–65.
    mark(&mut doc, 0, &[[85.0, 55.5, 105.0, 61.0]], "");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!(r.glyphs, 2, "CIDs 2 and 3");
    let c = content(&doc, 0);
    assert!(c.contains("[<0001> -750 <0004>] TJ") || c.contains("[(\\000\\001) -750 (\\000\\004)] TJ"), "{c}");
}

#[test]
fn images_are_removed_or_have_their_pixels_cleared() {
    // Image 1 is fully covered; image 2 (4×1 gray, all white) is half covered.
    let img = stream("/Type /XObject /Subtype /Image /Width 4 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", &[255, 255, 255, 255]);
    let src = b"q 10 0 0 10 10 10 cm /Im1 Do Q q 40 0 0 10 100 100 cm /Im2 Do Q";
    let mut doc = one_page(src, "/XObject << /Im1 7 0 R /Im2 7 0 R >>", vec![img]);
    mark(&mut doc, 0, &[[5.0, 5.0, 25.0, 25.0], [95.0, 95.0, 120.0, 115.0]], "");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!((r.images_removed, r.images_cleared), (1, 1), "{r:?}");
    let c = content(&doc, 0);
    assert!(!c.contains("/Im1 Do") && !c.contains("/Im2 Do"), "{c}");
    let p = pdfcraft_model::pages(&doc).swap_remove(0);
    let res = doc.resolve(p.dict.get(b"Resources").unwrap()).as_dict().cloned().unwrap();
    let xo = doc.resolve(res.get(b"XObject").unwrap()).as_dict().cloned().unwrap();
    let new = xo.iter().find(|(k, _)| k.starts_with(b"PCRedacted")).map(|(_, v)| v.clone()).expect("a cleared copy");
    let Object::Stream(s) = &*doc.resolve(&new) else { panic!() };
    assert_eq!(s.decoded().unwrap(), [0, 0, 255, 255], "pixels 1–2 (x 100–120) cleared");
    let Object::Stream(orig) = &*doc.get(ObjRef::new(7, 0)) else { panic!() };
    assert_eq!(orig.decoded().unwrap(), [255; 4], "the shared original is untouched");
}

#[test]
fn vectors_are_removed_or_clipped_and_inline_images_go() {
    let src = b"0 g 10 10 20 20 re f 0 0 300 300 re f q 10 0 0 10 50 50 cm BI /W 1 /H 1 /CS /G /BPC 8 ID \x80 EI Q 1 0 0 RG 150 150 m 160 160 l S";
    let mut doc = one_page(src, "", vec![]);
    mark(&mut doc, 0, &[[5.0, 5.0, 35.0, 35.0], [45.0, 45.0, 65.0, 65.0]], "");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!((r.paths_removed, r.paths_clipped, r.images_removed), (1, 1, 1), "{r:?}");
    let c = content(&doc, 0);
    assert!(!c.contains("10 10 20 20 re"), "{c}");
    assert!(c.contains("W*\nn\n0 0 300 300 re\nf\nQ"), "the page-size rect is clipped: {c}");
    assert!(!c.contains("BI"), "{c}");
    assert!(c.contains("150 150 m"), "the line elsewhere stays");
}

#[test]
fn shared_form_xobjects_are_copied_not_changed() {
    let form =
        stream("/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /Font << /F1 5 0 R >> >>", b"BT /F1 10 Tf 10 100 Td (SECRET) Tj ET");
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 8 0 R] /Count 2 /MediaBox [0 0 300 300] /Resources << /XObject << /Fm 7 0 R >> >> >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec(),
        stream("", b"/Fm Do"),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        form,
    ];
    objs.push(b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec());
    let mut doc = pdf(objs);
    mark(&mut doc, 0, &[[0.0, 90.0, 300.0, 120.0]], "");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!((r.glyphs, r.forms_rewritten), (6, 1), "{r:?}");
    assert!(content(&doc, 0).contains("/PCRedacted1 Do"));
    assert_eq!(content(&doc, 1), "/Fm Do", "page 2 shares the stream and the form: untouched");
    assert_eq!(under(&mut doc, 1, &[[0.0, 90.0, 300.0, 120.0]]), 6);
    assert_eq!(under(&mut doc, 0, &[[0.0, 90.0, 300.0, 120.0]]), 0);
}

#[test]
fn comments_links_and_fields_under_a_mark_go() {
    let mut doc = one_page(b"", "", vec![]);
    // A comment under the mark, a link elsewhere, and a text field under the mark.
    let square = Shape::Rectangle { rect: [20.0, 20.0, 40.0, 40.0] };
    add_annotation(
        &mut doc,
        &NewAnnotation { page: 0, style: Style::default_for(&square), shape: square, contents: "x".into(), author: "a".into() },
        &Meta::default(),
    )
    .unwrap();
    let link = Shape::Rectangle { rect: [200.0, 200.0, 220.0, 220.0] };
    add_annotation(
        &mut doc,
        &NewAnnotation { page: 0, style: Style::default_for(&link), shape: link, contents: "keep".into(), author: "a".into() },
        &Meta::default(),
    )
    .unwrap();
    pdfcraft_forms::add_field(&mut doc, 0, [10.0, 50.0, 100.0, 70.0], &pdfcraft_forms::NewField::Text { multiline: false }, Some("ssn")).unwrap();
    mark(&mut doc, 0, &[[0.0, 0.0, 120.0, 80.0]], "REDACTED");
    let r = apply(&mut doc, None).unwrap();
    assert_eq!((r.annotations, r.fields), (1, 1), "{r:?}");
    assert!(pdfcraft_forms::fields(&doc).is_empty());
    let p = pdfcraft_model::pages(&doc).swap_remove(0);
    assert_eq!(annots_of(&doc, &p.dict).len(), 1, "only the far rectangle stays");
    let c = content(&doc, 0);
    assert!(c.contains("(REDACTED) Tj"), "overlay text: {c}");
}

#[test]
fn nothing_to_apply_and_clearing_marks() {
    let mut doc = one_page(b"BT /F1 10 Tf 10 100 Td (AB) Tj ET", "", vec![]);
    assert_eq!(apply(&mut doc, None), Err(RedactError::NothingToApply));
    mark(&mut doc, 0, &[[0.0, 0.0, 50.0, 50.0]], "");
    assert_eq!(marks(&doc).len(), 1);
    assert_eq!(clear_marks(&mut doc, None), Ok(1));
    assert!(marks(&doc).is_empty());
    assert!(content(&doc, 0).contains("(AB) Tj"));
}

#[test]
fn unreadable_content_fails_closed() {
    let mut doc = one_page(b"", "", vec![]);
    let page = pdfcraft_model::pages(&doc)[0].obj;
    let bad = doc.add(Object::Stream(Stream::from_raw(
        {
            let mut d = Dict::new();
            d.set(b"Filter".to_vec(), Object::name("DCTDecode"));
            d
        },
        b"garbage".to_vec(),
    )));
    doc.update_dict(page, |d| d.set(b"Contents".to_vec(), Object::Ref(bad))).unwrap();
    mark(&mut doc, 0, &[[0.0, 0.0, 50.0, 50.0]], "");
    assert_eq!(apply(&mut doc, None), Err(RedactError::Unreadable(1)));
}

/// A document with something in every hidden-information category.
fn hidden_fixture() -> Document {
    let content = b"BT /F1 10 Tf 10 100 Td (Visible) Tj 3 Tr (Hidden) Tj 0 Tr ET BT /F1 10 Tf 500 500 Td (Offpage) Tj ET /OC /L1 BDC BT /F1 10 Tf 10 50 Td (Layer) Tj ET EMC";
    let objs: Vec<Vec<u8>> = vec![
        // 1 catalog
        b"<< /Type /Catalog /Pages 2 0 R /Metadata 7 0 R /Names << /EmbeddedFiles << /Names [(a.txt) 8 0 R] >> /JavaScript << /Names [(init) 9 0 R] >> >> /OpenAction 9 0 R /Outlines 10 0 R /PageMode /UseOutlines /PieceInfo << /App << /Private 1 >> >> /OCProperties << /OCGs [13 0 R 14 0 R] /D << /OFF [13 0 R] /Order [13 0 R 14 0 R] >> >> /AcroForm << /Fields [17 0 R] >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        // 3 page
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> /Properties << /L1 13 0 R >> >> /Annots [15 0 R 16 0 R 17 0 R 18 0 R 19 0 R] /PieceInfo << /App << /Private 2 >> >> /AA << /O 9 0 R >> >>".to_vec(),
        stream("", content),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta/>"),
        b"<< /Type /Filespec /F (a.txt) /EF << /F 20 0 R >> >>".to_vec(),
        b"<< /S /JavaScript /JS (app.alert(1)) >>".to_vec(),
        b"<< /Type /Outlines /First 11 0 R /Last 12 0 R /Count 2 >>".to_vec(),
        b"<< /Title (One) /Parent 10 0 R /Next 12 0 R >>".to_vec(),
        b"<< /Title (Two) /Parent 10 0 R /Prev 11 0 R >>".to_vec(),
        b"<< /Type /OCG /Name (Secret layer) >>".to_vec(),
        b"<< /Type /OCG /Name (Shown layer) >>".to_vec(),
        // 15 comment + 16 its pop-up, 17 widget, 18 link, 19 attachment
        b"<< /Type /Annot /Subtype /Square /Rect [10 10 50 50] /C [1 0 0] /Popup 16 0 R >>".to_vec(),
        b"<< /Type /Annot /Subtype /Popup /Rect [60 10 160 60] /Parent 15 0 R >>".to_vec(),
        b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /V (Ada) /Rect [10 200 150 220] /A 9 0 R >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [10 230 50 240] /A << /S /URI /URI (https://example.org) >> >>".to_vec(),
        b"<< /Type /Annot /Subtype /FileAttachment /Rect [200 10 210 20] /FS 8 0 R >>".to_vec(),
        stream("", b"attached"),
    ];
    let mut doc = pdf(objs);
    let info = doc.add(Object::Dict({
        let mut d = Dict::new();
        d.set(b"Title".to_vec(), pdfcraft_cos::PdfString::text("Secret plan"));
        d.set(b"Author".to_vec(), pdfcraft_cos::PdfString::text("Ada"));
        d
    }));
    doc.trailer_mut().set(b"Info".to_vec(), Object::Ref(info));
    doc
}

#[test]
fn hidden_information_is_counted_and_removed() {
    use crate::sanitize::{Hidden, remove_hidden, sanitize, scan};
    let mut doc = hidden_fixture();
    let counts: std::collections::HashMap<Hidden, usize> = scan(&doc).into_iter().collect();
    assert_eq!(counts[&Hidden::Metadata], 3, "two Info entries and the XMP stream");
    assert_eq!(counts[&Hidden::Attachments], 2, "the embedded file and the attachment annotation");
    assert_eq!(counts[&Hidden::Comments], 1);
    assert_eq!(counts[&Hidden::FormFields], 1);
    assert_eq!(counts[&Hidden::HiddenText], 6 + 7, "Hidden (render mode 3) and Offpage (off the page)");
    assert_eq!(counts[&Hidden::HiddenLayers], 2, "one off layer and its block");
    assert_eq!(counts[&Hidden::Bookmarks], 2);
    assert_eq!(counts[&Hidden::LinksActionsScripts], 5, "link, open action, page actions, widget action, document script");
    assert_eq!(counts[&Hidden::PrivateData], 2);

    // Only hidden text first.
    let done = remove_hidden(&mut doc, &[Hidden::HiddenText]).unwrap();
    assert_eq!(done, [(Hidden::HiddenText, 13)]);
    let c = content(&doc, 0);
    assert!(c.contains("(Visible)") && !c.contains("Hidden") && !c.contains("Offpage") && c.contains("(Layer)"), "{c}");
    assert!(doc.full_save_required());

    // Then everything.
    let done = sanitize(&mut doc).unwrap();
    assert!(done.iter().all(|(h, _)| *h != Hidden::HiddenText));
    let c = content(&doc, 0);
    assert!(!c.contains("Layer") && c.contains("(Visible)"), "{c}");
    let doc = reopen(&doc);
    let after: Vec<(Hidden, usize)> = scan(&doc).into_iter().filter(|(_, n)| *n > 0).collect();
    // A full save writes a fresh /Info with the modification date only.
    assert!(after.iter().all(|(h, _)| *h == Hidden::Metadata), "{after:?}");
    let cat = doc.get(doc.root().unwrap()).as_dict().cloned().unwrap();
    for k in [&b"Outlines"[..], b"OpenAction", b"PieceInfo", b"AcroForm", b"Metadata"] {
        assert!(!cat.contains(k), "{}", String::from_utf8_lossy(k));
    }
    assert!(pdfcraft_forms::fields(&doc).is_empty());
    let p = pdfcraft_model::pages(&doc).swap_remove(0);
    assert!(annots_of(&doc, &p.dict).is_empty());
    let shown = doc.object_numbers().into_iter().any(|n| match &*doc.get(ObjRef::new(n, doc.generation(n))) {
        Object::Stream(s) => s.decoded().is_ok_and(|d| d.windows(5).any(|w| w == b"(Ada)")),
        _ => false,
    });
    assert!(shown, "the field's value stays visible as page content");
}

#[test]
fn overlay_text_takes_its_font_size_colour_alignment_and_repeats() {
    let mut doc = one_page(b"BT /F1 12 Tf 20 250 Td (Secret salary figures) Tj ET", "", Vec::new());
    let look = pdfcraft_annot::OverlayLook { font: pdfcraft_annot::OverlayFont::Courier, size: 8.0, color: [0.0, 0.0, 1.0], align: 0, repeat: true };
    let shape = Shape::Redact { quads: vec![rect_quad([10.0, 200.0, 290.0, 270.0])], overlay: "REDACTED".into(), look };
    let style = Style::default_for(&shape);
    add_annotation(&mut doc, &NewAnnotation { page: 0, shape, style, contents: String::new(), author: "T".into() }, &Meta::default()).unwrap();
    // Written as Acrobat writes it.
    let m = &marks(&doc)[0];
    assert_eq!(m.look, look, "the look round-trips through /DA, /Q and /Repeat");
    apply(&mut doc, None).unwrap();
    let doc = reopen(&doc);
    let c = content(&doc, 0);
    assert!(c.contains("0 0 1 rg /PCCour 8 Tf"), "{c}");
    // 70 pt high at 8 pt × 1.2 leading: several lines, each the word repeated.
    assert!(c.matches(" Tm (REDACTED REDACTED").count() >= 5, "{c}");
    assert!(c.contains("1 0 0 1 11 "), "left aligned at the area's edge: {c}");
    let p = &pdfcraft_model::pages(&doc)[0];
    let fonts = p
        .dict
        .get(b"Resources")
        .and_then(|r| doc.resolve(r).as_dict().cloned())
        .and_then(|r| r.get(b"Font").and_then(|f| doc.resolve(f).as_dict().cloned()))
        .unwrap();
    assert!(fonts.contains(b"PCCour") && !fonts.contains(b"PCTimes"));
}

#[test]
fn tags_lose_what_redaction_removed() {
    let content = b"/P <</MCID 0>> BDC BT /F1 10 Tf 10 200 Td (SECRET) Tj ET EMC /P <</MCID 1>> BDC BT /F1 10 Tf 10 100 Td (Public) Tj ET EMC";
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 7 0 R /MarkInfo << /Marked true >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /StructParents 0 /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
        stream("", content),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        b"<< /Type /StructTreeRoot /K 8 0 R >>".to_vec(),
        b"<< /S /Document /P 7 0 R /K [9 0 R 10 0 R] >>".to_vec(),
        b"<< /S /P /P 8 0 R /Pg 3 0 R /ActualText (SECRET) /Alt (the secret) /K 0 >>".to_vec(),
        b"<< /S /P /P 8 0 R /Pg 3 0 R /ActualText (Public) /K 1 >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[[5.0, 195.0, 60.0, 212.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.tags, 1);
    let secret = doc.get(pdfcraft_cos::ObjRef::new(9, 0)).as_dict().cloned().unwrap();
    assert!(secret.get(b"ActualText").is_none() && secret.get(b"Alt").is_none());
    assert!(secret.get(b"K").is_none(), "its marked content is empty now");
    let public = doc.get(pdfcraft_cos::ObjRef::new(10, 0)).as_dict().cloned().unwrap();
    assert_eq!(public.get(b"K").and_then(Object::as_int), Some(1));
    assert!(public.get(b"ActualText").is_some(), "untouched content keeps its tags");
}

/// The page's content after applying, decoded and joined.
fn page_content(doc: &Document) -> String {
    let page = doc.get(pdfcraft_cos::ObjRef::new(3, 0)).as_dict().cloned().unwrap();
    let list: Vec<Object> = match page.get(b"Contents").cloned().unwrap() {
        Object::Array(items) => items,
        other => vec![other],
    };
    list.iter()
        .map(|item| match &*doc.resolve(item) {
            Object::Stream(s) => String::from_utf8_lossy(&s.decoded().unwrap()).into_owned(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn spans_lose_the_text_standing_for_what_redaction_removed() {
    // Embed build (R6b-47): a span whose glyphs were removed, and every level around it, lose
    // /ActualText, /Alt and /E, written in the content or named in the resources; a span left
    // whole keeps its own.
    let content = b"/Div <</ActualText (SECRET Public)>> BDC /Span <</ActualText (SECRET) /Alt (the secret) /E (SECRET) /Lang (en)>> BDC BT /F1 10 Tf 10 200 Td (SECRET) Tj ET EMC \
/Span /Named BDC BT /F1 10 Tf 10 150 Td (CLASSIFIED) Tj ET EMC /Span <</ActualText (Public)>> BDC BT /F1 10 Tf 10 100 Td (Public) Tj ET EMC EMC";
    let mut doc =
        one_page(content, "/Properties << /Named 7 0 R /Other << /ActualText (Other) >> >>", vec![b"<< /ActualText (CLASSIFIED) >>".to_vec()]);
    mark(&mut doc, 0, &[[5.0, 145.0, 120.0, 212.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.tags, 3, "the outer level, the inline span and the named one");
    let after = page_content(&doc);
    assert!(!after.contains("SECRET") && !after.contains("the secret") && !after.contains("CLASSIFIED"), "{after}");
    assert!(after.contains("/Lang (en)") && after.contains("/ActualText (Public)"), "{after}");
    let named = doc.get(pdfcraft_cos::ObjRef::new(7, 0)).as_dict().cloned().unwrap();
    assert!(named.get(b"ActualText").is_none(), "the named list loses it where it is");
}

#[test]
fn a_named_list_written_in_the_resources_loses_its_text_there() {
    let content = b"/Span /Named BDC BT /F1 10 Tf 10 200 Td (SECRET) Tj ET EMC /Span /Kept BDC BT /F1 10 Tf 10 100 Td (Public) Tj ET EMC";
    let mut doc = one_page(content, "/Properties << /Named << /ActualText (SECRET) >> /Kept << /ActualText (Public) >> >>", vec![]);
    mark(&mut doc, 0, &[[5.0, 195.0, 60.0, 212.0]], "");
    apply(&mut doc, None).unwrap();
    let page = doc.get(pdfcraft_cos::ObjRef::new(3, 0)).as_dict().cloned().unwrap();
    let resources = doc.resolve(page.get(b"Resources").unwrap()).as_dict().cloned().unwrap();
    let properties = doc.resolve(resources.get(b"Properties").unwrap()).as_dict().cloned().unwrap();
    let list = |name: &[u8]| doc.resolve(properties.get(name).unwrap()).as_dict().cloned().unwrap();
    assert!(list(b"Named").get(b"ActualText").is_none());
    assert!(list(b"Kept").get(b"ActualText").is_some());
}

#[test]
fn ancestors_and_named_marked_content_lose_their_text_too() {
    // Embed build (R6b-47): the section around the paragraph whose content changed loses its
    // text as well; a named property list's MCID counts; a paragraph naming no page is this
    // page's.
    let content = b"/P /MC0 BDC BT /F1 10 Tf 10 200 Td (SECRET) Tj ET EMC /P <</MCID 1>> BDC BT /F1 10 Tf 10 100 Td (Public) Tj ET EMC";
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 7 0 R /MarkInfo << /Marked true >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /StructParents 0 /Resources << /Font << /F1 5 0 R >> /Properties << /MC0 << /MCID 0 >> >> >> >>".to_vec(),
        stream("", content),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        b"<< /Type /StructTreeRoot /K 8 0 R >>".to_vec(),
        b"<< /S /Sect /P 7 0 R /Alt (A section about the SECRET) /K [9 0 R 10 0 R] >>".to_vec(),
        b"<< /S /P /P 8 0 R /ActualText (SECRET) /K 0 >>".to_vec(),
        b"<< /S /P /P 8 0 R /ActualText (Public) /K 1 >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[[5.0, 195.0, 60.0, 212.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.tags, 2, "the paragraph and its section");
    let element = |num: u32| doc.get(pdfcraft_cos::ObjRef::new(num, 0)).as_dict().cloned().unwrap();
    assert!(element(8).get(b"Alt").is_none() && element(8).get(b"K").is_some(), "the section keeps its kids");
    assert!(element(9).get(b"ActualText").is_none());
    assert!(element(10).get(b"ActualText").is_some());
}

// ── Embed build: the follow-up 6 audit of the redactor ──────────────────────────────────────

/// Every string the document's file holds once written whole (reachable objects only), decoded
/// streams included, as one lossy string.
fn written_text(doc: &Document) -> String {
    let bytes = write_full(doc, &SaveOptions::default()).unwrap();
    let doc = Document::open(Arc::new(bytes)).unwrap();
    let size = doc.trailer().int(b"Size").unwrap();
    let mut out = String::new();
    fn walk(object: &Object, out: &mut String) {
        match object {
            Object::String(s) => out.push_str(&String::from_utf8_lossy(&s.bytes)),
            Object::Array(items) => items.iter().for_each(|item| walk(item, out)),
            Object::Dict(dict) => dict.iter().for_each(|(_, value)| walk(value, out)),
            Object::Stream(stream) => {
                stream.dict.iter().for_each(|(_, value)| walk(value, out));
                out.push_str(&String::from_utf8_lossy(&stream.decoded().unwrap()));
            }
            _ => {}
        }
        out.push('\n');
    }
    for num in 1..u32::try_from(size).unwrap() {
        if let Ok(object) = doc.try_get(num) {
            walk(&object, &mut out);
        }
    }
    out
}

/// A font whose every glyph is 1,000 units wide (a 10 pt glyph advances 10 pt), widths object
/// `widths`.
fn wide_font(widths: u32) -> Vec<u8> {
    format!("<< /Type /Font /Subtype /TrueType /BaseFont /Wide /FirstChar 32 /LastChar 126 /Widths {widths} 0 R >>").into_bytes()
}

fn wide_widths() -> Vec<u8> {
    format!("[{}]", vec!["1000"; 95].join(" ")).into_bytes()
}

#[test]
fn a_form_finds_its_font_through_the_page_s_resources() {
    // A form with resources of its own but no /Font draws with the page's font, as hayro looks
    // resources up. The redactor placed its glyphs with fallback metrics and left "J" (at x
    // 100–110 in the real font) under the box.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F2 5 0 R >> /XObject << /Fm1 7 0 R >> >> >>"
            .to_vec(),
        stream("", b"/Fm1 Do"),
        wide_font(6),
        wide_widths(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /ProcSet [/PDF /Text] >>",
            b"BT /F2 10 Tf 10 200 Td (ABCDEFGHIJ) Tj ET",
        ),
    ]);
    mark(&mut doc, 0, &[[101.0, 195.0, 112.0, 212.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.glyphs, 1, "{report:?}");
    let written = written_text(&doc);
    assert!(written.contains("ABCDEFGHI") && !written.contains("ABCDEFGHIJ"), "{written}");
}

#[test]
fn a_form_finds_its_image_through_the_page_s_resources() {
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Fm1 5 0 R /Im1 6 0 R >> >> >>".to_vec(),
        stream("", b"/Fm1 Do"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /ProcSet [/PDF] >>", b"q 50 0 0 50 100 100 cm /Im1 Do Q"),
        stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", b"\x80"),
    ]);
    mark(&mut doc, 0, &[[90.0, 90.0, 160.0, 160.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.images_removed, 1, "{report:?}");
}

/// A Type 0 font with `encoding` (a CMap name), its glyphs 1,000 units each way.
fn cjk_font(encoding: &str) -> Vec<u8> {
    format!(
        "<< /Type /Font /Subtype /Type0 /BaseFont /HeiseiMin-W3 /Encoding /{encoding} /DescendantFonts [<< /Type /Font /Subtype /CIDFontType0 \
/BaseFont /HeiseiMin-W3 /CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 2 >> /DW 1000 >>] >>"
    )
    .into_bytes()
}

#[test]
fn text_the_redactor_cannot_place_near_a_mark_refuses_the_redaction() {
    // Vertical writing (and CMaps it does not read) runs glyphs where the redactor's horizontal,
    // two-byte model does not put them: "glyph 4" at (100, 220) was left under the box.
    for encoding in ["Identity-V", "UniJIS-UCS2-V", "90ms-RKSJ-H"] {
        let content = b"BT /F2 10 Tf 100 250 Td <0001000200030004> Tj ET";
        let make = || {
            pdf(vec![
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F2 5 0 R >> >> >>".to_vec(),
                stream("", content),
                cjk_font(encoding),
            ])
        };
        let mut doc = make();
        mark(&mut doc, 0, &[[90.0, 212.0, 112.0, 228.0]], "");
        assert!(matches!(apply(&mut doc, None), Err(RedactError::Residue(_))), "{encoding}");
        // Far from the text, the mark is applied.
        let mut doc = make();
        mark(&mut doc, 0, &[[5.0, 5.0, 25.0, 25.0]], "");
        assert!(apply(&mut doc, None).is_ok(), "{encoding}");
    }
    // The horizontal identity CMap is placed as before.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F2 5 0 R >> >> >>".to_vec(),
        stream("", b"BT /F2 10 Tf 100 250 Td <0001000200030004> Tj ET"),
        cjk_font("Identity-H"),
    ]);
    mark(&mut doc, 0, &[[121.0, 245.0, 129.0, 262.0]], "");
    assert_eq!(apply(&mut doc, None).unwrap().glyphs, 1);
}

#[test]
fn what_a_redaction_replaced_is_not_left_in_the_resources() {
    // A form whose text was redacted is drawn under a new name; the old name kept the original
    // form, "SECRET" and all, reachable in the written file. Likewise a cleared or removed image.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> /XObject << /Fm1 7 0 R /Im1 8 0 R >> >> >>".to_vec(),
        stream("", b"/Fm1 Do q 50 0 0 50 200 200 cm /Im1 Do Q"),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 300 300]", b"BT /F1 10 Tf 10 100 Td (SECRET) Tj ET BT /F1 10 Tf 10 50 Td (Public) Tj ET"),
        stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", b"PIXELSECRET"),
    ]);
    mark(&mut doc, 0, &[[5.0, 95.0, 60.0, 112.0], [190.0, 190.0, 260.0, 260.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!((report.glyphs, report.images_removed), (6, 1), "{report:?}");
    let written = written_text(&doc);
    assert!(!written.contains("SECRET"), "{written}");
    assert!(written.contains("Public"));
}

#[test]
fn a_tagged_form_s_marked_content_is_cleaned_and_its_original_not_kept() {
    // The structure tree names a form's marked content through /Stm: the element kept its text
    // and the reference kept the original form, "SECRET" and all, in the written file.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 8 0 R /MarkInfo << /Marked true >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /StructParents 0 /Resources << /Font << /F1 5 0 R >> /XObject << /Fm1 7 0 R >> >> >>".to_vec(),
        stream("", b"/Fm1 Do"),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /StructParents 1",
            b"/P <</MCID 0>> BDC BT /F1 10 Tf 10 200 Td (SECRET) Tj ET EMC /P <</MCID 1>> BDC BT /F1 10 Tf 10 100 Td (Public) Tj ET EMC",
        ),
        b"<< /Type /StructTreeRoot /K [9 0 R 10 0 R] >>".to_vec(),
        b"<< /S /P /P 8 0 R /Pg 3 0 R /ActualText (SECRET) /K << /Type /MCR /Pg 3 0 R /Stm 7 0 R /MCID 0 >> >>".to_vec(),
        b"<< /S /P /P 8 0 R /Pg 3 0 R /ActualText (Public) /K << /Type /MCR /Pg 3 0 R /Stm 7 0 R /MCID 1 >> >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[[5.0, 195.0, 60.0, 212.0]], "");
    apply(&mut doc, None).unwrap();
    let written = written_text(&doc);
    assert!(!written.contains("SECRET"), "{written}");
    let public = doc.get(pdfcraft_cos::ObjRef::new(10, 0)).as_dict().cloned().unwrap();
    assert!(public.get(b"ActualText").is_some(), "untouched content keeps its tags");
}

#[test]
fn one_mcid_on_two_pages_cleans_only_the_redacted_page_s_element() {
    let page = |word: &str| format!("/P <</MCID 0>> BDC BT /F1 10 Tf 10 200 Td ({word}) Tj ET EMC").into_bytes();
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 5 0 R /StructParents 0 /Resources << /Font << /F1 7 0 R >> >> >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 6 0 R /StructParents 1 /Resources << /Font << /F1 7 0 R >> >> >>".to_vec(),
        stream("", &page("SECRET")),
        stream("", &page("Keep")),
        FONT.replace("95 0 R", "8 0 R").into_bytes(),
        widths(),
        b"<< /Type /StructTreeRoot /K [10 0 R 11 0 R 12 0 R] >>".to_vec(),
        b"<< /S /P /P 9 0 R /Pg 3 0 R /ActualText (SECRET) /K 0 >>".to_vec(),
        b"<< /S /P /P 9 0 R /Pg 4 0 R /ActualText (Keep) /K 0 >>".to_vec(),
        // Names no page: it may be the first page's, so it goes too.
        b"<< /S /Figure /P 9 0 R /Alt (A SECRET figure) /K 0 >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[[5.0, 195.0, 60.0, 212.0]], "");
    apply(&mut doc, None).unwrap();
    let element = |num: u32| doc.get(pdfcraft_cos::ObjRef::new(num, 0)).as_dict().cloned().unwrap();
    assert!(element(10).get(b"ActualText").is_none());
    assert!(element(11).get(b"ActualText").is_some(), "the other page's MCID 0 is its own");
    assert!(element(12).get(b"Alt").is_none());
}

#[test]
fn unbalanced_marked_content_takes_every_span_s_text_away() {
    // An `EMC` closing nothing: which glyphs a span holds is each reader's guess, so every span
    // of the scope, and every element of the page's marked content, loses its text.
    let content = b"/Span <</ActualText (SECRET)>> BDC BT /F1 10 Tf 10 200 Td (SECRET) Tj ET EMC EMC /P <</MCID 0 /ActualText (Public)>> BDC BT /F1 10 Tf 10 100 Td (Public) Tj ET EMC";
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 7 0 R /MarkInfo << /Marked true >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /StructParents 0 /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
        stream("", content),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        b"<< /Type /StructTreeRoot /K 8 0 R >>".to_vec(),
        b"<< /S /P /P 7 0 R /Pg 3 0 R /ActualText (Public) /K 0 >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[[5.0, 195.0, 60.0, 212.0]], "");
    apply(&mut doc, None).unwrap();
    let after = page_content(&doc);
    assert!(!after.contains("ActualText"), "{after}");
    let element = doc.get(pdfcraft_cos::ObjRef::new(8, 0)).as_dict().cloned().unwrap();
    assert!(element.get(b"ActualText").is_none());
}

#[test]
fn an_annotation_removed_under_a_mark_takes_its_tags_text_and_reference() {
    // Embed build (follow-up 7 audit, item 10): the link under the mark goes; its element named
    // it through an object reference, with /Alt spelling out the address.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 7 0 R /MarkInfo << /Marked true >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Annots [10 0 R] /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
        stream("", b"BT /F1 10 Tf 10 200 Td (SECRET) Tj ET"),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        b"<< /Type /StructTreeRoot /K 8 0 R >>".to_vec(),
        b"<< /S /Sect /P 7 0 R /Alt (About the SECRET) /K 9 0 R >>".to_vec(),
        b"<< /S /Link /P 8 0 R /Pg 3 0 R /Alt (https://secret.example) /K << /Type /OBJR /Obj 10 0 R >> >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [8 195 60 212] /A << /S /URI /URI (https://secret.example) >> >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[[5.0, 195.0, 60.0, 212.0]], "");
    apply(&mut doc, None).unwrap();
    let element = |num: u32| doc.get(pdfcraft_cos::ObjRef::new(num, 0)).as_dict().cloned().unwrap();
    assert!(element(9).get(b"Alt").is_none() && element(9).get(b"K").is_none(), "{:?}", element(9));
    assert!(element(8).get(b"Alt").is_none());
    assert!(!written_text(&doc).contains("secret.example"));
}

// ── Embed build: follow-up 8 (R6b-50), the redactor made complete ────────────────────────────

/// Every byte the document holds once written whole, with each stream's data decoded, so that
/// names, keys and strings are all searched.
fn written_bytes(doc: &Document) -> Vec<u8> {
    let bytes = write_full(doc, &SaveOptions::default()).unwrap();
    let reopened = Document::open(Arc::new(bytes.clone())).unwrap();
    let mut out = bytes;
    for num in reopened.object_numbers() {
        if let Ok(object) = reopened.try_get(num)
            && let Object::Stream(stream) = &*object
            && let Ok(data) = stream.decoded()
        {
            out.extend_from_slice(&data);
        }
    }
    out
}

fn holds(doc: &Document, word: &[u8]) -> bool {
    written_bytes(doc).windows(word.len()).any(|window| window == word)
}

const SECRET_LINE: &[u8] = b"BT /F1 10 Tf 10 100 Td (SECRET) Tj ET BT /F1 10 Tf 10 200 Td (Public) Tj ET";
const OVER_SECRET: [f64; 4] = [5.0, 95.0, 45.0, 110.0];

#[test]
fn structure_titles_go_with_the_content_they_name() {
    // Re-review M1: an element's /T was kept.
    let content = b"/P <</MCID 0>> BDC BT /F1 10 Tf 10 100 Td (SECRET) Tj ET EMC";
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 7 0 R /MarkInfo << /Marked true >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /StructParents 0 /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
        stream("", content),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        b"<< /Type /StructTreeRoot /K 8 0 R >>".to_vec(),
        b"<< /S /P /P 7 0 R /Pg 3 0 R /T (SECRET title) /K 0 >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[OVER_SECRET], "");
    apply(&mut doc, None).unwrap();
    assert!(!holds(&doc, b"SECRET"));
}

/// A 4 × 4 grey image with a 4 × 4 soft mask and the entries `extra`, drawn over 10–50 × 90–130.
fn masked_image(extra: &str) -> Document {
    let image = format!("/Type /XObject /Subtype /Image /Width 4 /Height 4 /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 8 0 R {extra}");
    one_page(
        b"q 40 0 0 40 10 90 cm /Im Do Q",
        "/XObject << /Im 7 0 R >>",
        vec![
            stream(&image, &[200u8; 16]),
            stream("/Type /XObject /Subtype /Image /Width 4 /Height 4 /ColorSpace /DeviceGray /BitsPerComponent 8", b"SECRETSECRETSECR"),
            stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta>SECRET</x:xmpmeta>"),
            b"<< /Private (SECRET) >>".to_vec(),
            stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", b"S"),
        ],
    )
}

#[test]
fn redacted_copies_keep_nothing_of_their_originals_but_what_shows() {
    // Re-review I2: a cleared image kept its soft mask whole, its alternates, its XMP and its
    // private data; a rewritten form kept its XMP and private data.
    let mut doc = masked_image("/Metadata 9 0 R /PieceInfo << /App 10 0 R >> /Alternates [<< /Image 11 0 R >>] /OPI << /Version (SECRET) >>");
    // The left half of the image (x 10–30).
    mark(&mut doc, 0, &[[8.0, 88.0, 30.0, 132.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.images_cleared, 1);
    assert!(!holds(&doc, b"SECRET"), "the copy keeps the original's entries or its mask's covered samples");
    // A form drawing the word, with its XMP and private data.
    let mut doc = one_page(
        b"/Fm Do",
        "/XObject << /Fm 7 0 R >>",
        vec![
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Metadata 8 0 R /PieceInfo << /App << /Private (SECRET) >> >> /LastModified (SECRET)",
                SECRET_LINE,
            ),
            stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta>SECRET</x:xmpmeta>"),
        ],
    );
    mark(&mut doc, 0, &[OVER_SECRET], "");
    apply(&mut doc, None).unwrap();
    assert!(!holds(&doc, b"SECRET"));
}

#[test]
fn a_redacted_page_loses_its_thumbnail_and_private_data() {
    // Re-review I3 and I5: the page's /Thumb (a picture of it as it was), its private data and
    // XMP were kept.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> /Thumb 7 0 R /PieceInfo << /App << /Private (SECRET) >> >> /LastModified (D:2020) /Metadata 8 0 R >>".to_vec(),
        stream("", SECRET_LINE),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        stream("/Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", b"S"),
        stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta>SECRET</x:xmpmeta>"),
    ]);
    mark(&mut doc, 0, &[OVER_SECRET], "");
    let report = apply(&mut doc, None).unwrap();
    let page = doc.get(pdfcraft_cos::ObjRef::new(3, 0)).as_dict().cloned().unwrap();
    for key in [&b"Thumb"[..], b"PieceInfo", b"LastModified", b"Metadata"] {
        assert!(!page.contains(key), "{}", String::from_utf8_lossy(key));
    }
    assert_eq!(report.thumbnails, 1);
    assert!(!holds(&doc, b"SECRET"));
}

#[test]
fn replies_to_a_comment_under_a_mark_go_with_it() {
    // Re-review I4: a reply elsewhere kept the removed comment reachable through its /IRT.
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> /Annots [7 0 R 8 0 R 9 0 R 10 0 R 11 0 R] >>".to_vec(),
        stream("", SECRET_LINE),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        b"<< /Type /Annot /Subtype /FreeText /Rect [10 98 40 108] /Contents (SECRET) /DA (/Helv 10 Tf 0 g) /Popup 8 0 R >>".to_vec(),
        b"<< /Type /Annot /Subtype /Popup /Rect [200 200 280 260] /Parent 7 0 R >>".to_vec(),
        b"<< /Type /Annot /Subtype /Text /Rect [250 10 270 30] /IRT 7 0 R /Contents (About SECRET) >>".to_vec(),
        b"<< /Type /Annot /Subtype /Text /Rect [250 40 270 60] /IRT 9 0 R /Contents (Agreed) >>".to_vec(),
        b"<< /Type /Annot /Subtype /Text /Rect [250 70 270 90] /Contents (Unrelated) >>".to_vec(),
    ]);
    mark(&mut doc, 0, &[OVER_SECRET], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.annotations, 3, "the comment and its thread");
    let page = doc.get(pdfcraft_cos::ObjRef::new(3, 0)).as_dict().cloned().unwrap();
    let kept: Vec<Object> = page.get(b"Annots").and_then(Object::as_array).cloned().unwrap_or_default();
    assert_eq!(kept, vec![Object::Ref(pdfcraft_cos::ObjRef::new(11, 0))]);
    assert!(!holds(&doc, b"SECRET"));
}

#[test]
fn pattern_cells_and_soft_mask_groups_under_a_mark_are_redacted() {
    // Re-review I1: the redactor never entered a tiling pattern's cell or a soft mask's group.
    let pattern = "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 300 300] /XStep 300 /YStep 300 /Resources << /Font << /F1 5 0 R >> >>";
    let mut doc = one_page(
        b"q /Pattern cs /P1 scn 0 0 300 300 re f Q BT /F1 10 Tf 10 200 Td (Public) Tj ET",
        "/Pattern << /P1 7 0 R >>",
        vec![stream(pattern, b"BT /F1 10 Tf 10 100 Td (SECRET) Tj ET BT /F1 10 Tf 10 250 Td (Tiles) Tj ET")],
    );
    mark(&mut doc, 0, &[OVER_SECRET], "");
    apply(&mut doc, None).unwrap();
    assert!(!holds(&doc, b"SECRET"), "a pattern's cell");
    assert!(holds(&doc, b"Tiles"), "what the cell shows elsewhere stays");
    let group = "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Group << /S /Transparency >> /Resources << /Font << /F1 5 0 R >> >>";
    let mut doc = one_page(
        b"q /Mask gs 0 g 0 0 300 300 re f Q BT /F1 10 Tf 10 200 Td (Public) Tj ET",
        "/ExtGState << /Mask << /Type /ExtGState /SMask << /S /Luminosity /G 7 0 R >> >> >>",
        vec![stream(group, b"1 g BT /F1 10 Tf 10 100 Td (SECRET) Tj ET")],
    );
    mark(&mut doc, 0, &[OVER_SECRET], "");
    apply(&mut doc, None).unwrap();
    assert!(!holds(&doc, b"SECRET"), "a soft mask's group");
}

#[test]
fn a_resource_the_redacted_content_no_longer_uses_goes() {
    // Re-review M2: a font used only by the removed word stayed, its subset and map spelling it.
    let mut doc = one_page(
        b"BT /F2 10 Tf 10 100 Td (SECRET) Tj ET BT /F1 10 Tf 10 200 Td (Public) Tj ET",
        "/Font << /F1 5 0 R /F2 7 0 R >>",
        vec![FONT.replace("95 0 R", "6 0 R").replace("/Arial", "/SECRET-Bold").into_bytes()],
    );
    mark(&mut doc, 0, &[OVER_SECRET], "");
    apply(&mut doc, None).unwrap();
    let page = doc.get(pdfcraft_cos::ObjRef::new(3, 0)).as_dict().cloned().unwrap();
    let fonts = page
        .get(b"Resources")
        .and_then(|r| doc.resolve(r).as_dict().cloned())
        .and_then(|r| r.get(b"Font").and_then(|f| doc.resolve(f).as_dict().cloned()))
        .unwrap();
    assert!(fonts.contains(b"F1") && !fonts.contains(b"F2"), "{fonts:?}");
    assert!(!holds(&doc, b"SECRET"));
}

#[test]
fn copies_of_the_removed_words_elsewhere_in_the_document_are_cleared() {
    // Re-review I5 (R6b-50): bookmark titles holding a removed word read "[Redacted]", document
    // information values holding one are cleared, the XMP packet goes, and a named destination
    // whose name holds one is renamed (with what goes to it).
    let mut doc = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R /Outlines 7 0 R /Dests << /SECRET-clause [3 0 R /Fit] /Other [3 0 R /Fit] >> /Metadata 9 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> /Annots [11 0 R] >>".to_vec(),
        stream("", SECRET_LINE),
        FONT.replace("95 0 R", "6 0 R").into_bytes(),
        widths(),
        b"<< /Type /Outlines /First 8 0 R /Last 8 0 R /Count 1 >>".to_vec(),
        b"<< /Title (The SECRET clause) /Parent 7 0 R /Dest /SECRET-clause >>".to_vec(),
        stream("/Type /Metadata /Subtype /XML", b"<x:xmpmeta>SECRET</x:xmpmeta>"),
        b"<< /Title (Public title) /Subject (About SECRET) >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [200 200 250 220] /Dest /SECRET-clause >>".to_vec(),
    ]);
    doc.trailer_mut().set(b"Info".to_vec(), Object::Ref(pdfcraft_cos::ObjRef::new(10, 0)));
    mark(&mut doc, 0, &[OVER_SECRET], "");
    let report = apply_with(&mut doc, None, Options { black: true, document_copies: true }).unwrap();
    assert!(!holds(&doc, b"SECRET"));
    assert!(holds(&doc, b"Public title") && holds(&doc, b"[Redacted]") && holds(&doc, b"Other"));
    assert_eq!((report.bookmarks_cleared, report.information_cleared, report.metadata_removed, report.destinations_renamed), (1, 1, 1, 1));
    // The bookmark and the link still go where the name went.
    let catalog = doc.get(doc.root().unwrap()).as_dict().cloned().unwrap();
    let dests = catalog.get(b"Dests").and_then(|d| doc.resolve(d).as_dict().cloned()).unwrap();
    let link = doc.get(pdfcraft_cos::ObjRef::new(11, 0)).as_dict().cloned().unwrap();
    let renamed = link.get(b"Dest").and_then(Object::as_name).unwrap().to_vec();
    assert!(dests.contains(&renamed), "{dests:?}");
}

#[test]
fn applied_for_a_host_every_area_is_filled_black_and_overlays_are_ignored() {
    // P6-33: the redactor ignores a mark's overlay entries and fills each area solid black.
    let mut doc = one_page(SECRET_LINE, "", vec![]);
    let shape = Shape::Redact { quads: vec![rect_quad(OVER_SECRET)], overlay: "Withheld".into(), look: Default::default() };
    let mut style = Style::default_for(&shape);
    style.fill = Some([1.0, 0.0, 0.0]);
    add_annotation(&mut doc, &NewAnnotation { page: 0, shape, style, contents: String::new(), author: "Tester".into() }, &Meta::default()).unwrap();
    apply_with(&mut doc, None, Options { black: true, document_copies: false }).unwrap();
    let c = content(&doc, 0);
    assert!(c.contains("0 0 0 rg") && !c.contains("1 0 0 rg") && !c.contains("Withheld"), "{c}");
}

// ── Embed build: follow-up 8 audit of the interpreter ───────────────────────────────────────

#[test]
fn a_glyph_a_mark_reaches_at_all_goes() {
    // Follow-up 8 audit: a glyph only partly under a mark (its box reaching a little way in)
    // was kept, part of what was marked readable.
    let mut doc = one_page(b"BT /F1 10 Tf 10 100 Td (AB) Tj ET", "", vec![]);
    // A at 10–15, B at 15–20: the mark reaches 0.5 pt into A.
    mark(&mut doc, 0, &[[14.5, 95.0, 20.0, 110.0]], "");
    let report = apply(&mut doc, None).unwrap();
    assert_eq!(report.glyphs, 2, "{report:?}");
}

/// `levels` forms, each drawing the next, the last showing "SECRET" under the mark.
fn nested_forms(levels: u32) -> Document {
    let mut extra = Vec::new();
    for level in 0..levels {
        let num = 7 + level;
        let content = if level + 1 == levels { b"BT /F1 10 Tf 10 100 Td (SECRET) Tj ET".to_vec() } else { b"/Fm Do".to_vec() };
        extra.push(stream(
            &format!("/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /Font << /F1 5 0 R >> /XObject << /Fm {} 0 R >> >>", num + 1),
            &content,
        ));
    }
    one_page(b"/Fm Do", "/XObject << /Fm 7 0 R >>", extra)
}

/// `levels` tiling patterns, each painting the next over its cell, the last showing "SECRET".
fn nested_patterns(levels: u32) -> Document {
    let mut extra = Vec::new();
    for level in 0..levels {
        let num = 7 + level;
        let content =
            if level + 1 == levels { b"BT /F1 10 Tf 10 100 Td (SECRET) Tj ET".to_vec() } else { b"/Pattern cs /P scn 0 0 300 300 re f".to_vec() };
        extra.push(stream(
            &format!(
                "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 300 300] /XStep 300 /YStep 300 /Resources << /Font << /F1 5 0 R >> /Pattern << /P {} 0 R >> >>",
                num + 1
            ),
            &content,
        ));
    }
    one_page(b"/Pattern cs /P scn 0 0 300 300 re f", "/Pattern << /P 7 0 R >>", extra)
}

/// `levels` soft masks, each group setting the next, the last showing "SECRET".
fn nested_masks(levels: u32) -> Document {
    let mut extra = Vec::new();
    for level in 0..levels {
        let num = 7 + level;
        let content = if level + 1 == levels { b"BT /F1 10 Tf 10 100 Td (SECRET) Tj ET".to_vec() } else { b"/M gs 0 0 300 300 re f".to_vec() };
        extra.push(stream(
            &format!(
                "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Group << /S /Transparency >> /Resources << /Font << /F1 5 0 R >> /ExtGState << /M << /SMask << /S /Luminosity /G {} 0 R >> >> >> >>",
                num + 1
            ),
            &content,
        ));
    }
    one_page(b"/M gs 0 0 300 300 re f", "/ExtGState << /M << /SMask << /S /Luminosity /G 7 0 R >> >> >>", extra)
}

#[test]
fn nesting_past_the_bound_fails_the_redaction_quickly() {
    // Follow-up 8 audit: forms, patterns and soft masks nest through one depth bound; past it
    // the redaction fails (nothing changed), within a small time.
    for (kind, within, past) in [
        ("forms", nested_forms(MAX_NESTING), nested_forms(MAX_NESTING + 1)),
        ("patterns", nested_patterns(MAX_NESTING), nested_patterns(MAX_NESTING + 1)),
        ("masks", nested_masks(MAX_NESTING), nested_masks(MAX_NESTING + 1)),
    ] {
        let mut within = within;
        mark(&mut within, 0, &[OVER_SECRET], "");
        assert!(apply(&mut within, None).is_ok(), "{kind} within the bound");
        let mut past = past;
        mark(&mut past, 0, &[OVER_SECRET], "");
        let started = std::time::Instant::now();
        assert!(apply(&mut past, None).is_err(), "{kind} past the bound");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{kind}");
    }
}

#[test]
fn work_past_the_bound_fails_the_redaction() {
    // Follow-up 8 audit: a shared work counter bounds the whole redaction, whatever the input:
    // 200,000 glyphs each tested against 3,000 marks.
    let mut content = b"BT /F1 1 Tf 0 0 Td (".to_vec();
    content.extend(std::iter::repeat_n(b'S', 200_000));
    content.extend_from_slice(b") Tj ET");
    let mut doc = one_page(&content, "", vec![]);
    let marks: Vec<[f64; 4]> = (0..3_000)
        .map(|i| [f64::from(i % 60) * 4.0 + 1.0, f64::from(i / 60) * 4.0 + 50.0, f64::from(i % 60) * 4.0 + 2.0, f64::from(i / 60) * 4.0 + 51.0])
        .collect();
    mark(&mut doc, 0, &marks, "");
    let started = std::time::Instant::now();
    assert_eq!(apply(&mut doc, None), Err(RedactError::TooComplex));
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "{:?}", started.elapsed());
}

#[test]
fn state_the_redactor_cannot_track_exactly_refuses_the_redaction() {
    // Follow-up 8 audit: where the interpreter's state could drift from a viewer's (a `Q` with
    // no `q`, text shown outside a text object, an operator with operands it does not take, a
    // Type 3 font whose matrix is not a plain scale), glyph places are guesses: refused.
    let type3 = "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] /FontMatrix [0.001 0.0005 0 0.001 0 0] /CharProcs << /a 8 0 R >> /Encoding << /Type /Encoding /Differences [83 /a] >> /FirstChar 83 /LastChar 83 /Widths [1000] /Resources << >> >>";
    for (case, content, extra_res, extra) in [
        ("a Q without q", &b"Q BT /F1 10 Tf 10 100 Td (SECRET) Tj ET"[..], "", vec![]),
        ("text outside a text object", &b"BT /F1 10 Tf 10 100 Td ET (SECRET) Tj"[..], "", vec![]),
        ("Td with three operands", &b"BT /F1 10 Tf 5 10 100 Td (SECRET) Tj ET"[..], "", vec![]),
        ("cm with a name", &b"2 0 0 2 0 0 /Junk cm BT /F1 10 Tf 10 100 Td (SECRET) Tj ET"[..], "", vec![]),
        ("Tz without operand", &b"BT /F1 10 Tf Tz 10 100 Td (SECRET) Tj ET"[..], "", vec![]),
        (
            "a skewed Type 3 font",
            &b"BT /T3 10 Tf 10 100 Td (SSSSSS) Tj ET"[..],
            "/Font << /F1 5 0 R /T3 7 0 R >>",
            vec![type3.as_bytes().to_vec(), stream("", b"1000 0 d0 0 0 1000 1000 re f")],
        ),
    ] {
        let mut doc = if extra_res.is_empty() {
            one_page(content, "", vec![])
        } else {
            one_page(content, &extra_res.replace("/Font << /F1 5 0 R", "/XFont << /F1 5 0 R"), extra)
        };
        if !extra_res.is_empty() {
            // The page names its own fonts: written again with both.
            let page = doc.get(pdfcraft_cos::ObjRef::new(3, 0)).as_dict().cloned().unwrap();
            let mut resources = Dict::new();
            let mut fonts = Dict::new();
            fonts.set(b"F1".to_vec(), Object::Ref(pdfcraft_cos::ObjRef::new(5, 0)));
            fonts.set(b"T3".to_vec(), Object::Ref(pdfcraft_cos::ObjRef::new(7, 0)));
            resources.set(b"Font".to_vec(), Object::Dict(fonts));
            let mut page = page;
            page.set(b"Resources".to_vec(), Object::Dict(resources));
            doc.set(pdfcraft_cos::ObjRef::new(3, 0), Object::Dict(page));
        }
        mark(&mut doc, 0, &[OVER_SECRET], "");
        assert!(apply(&mut doc, None).is_err(), "{case}");
    }
}
