//! Embed build: the documented limits apply to inputs whose headers declare large sizes but that
//! are small in memory. Each case is a rewrite of a small page; each ends with
//! [`InspectError::TooComplex`] or a bounded result, measured without a clock (the deadline is an
//! hour, so the work bounds are what stop it).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use super::*;

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

fn flate(dict: &str, data: &[u8]) -> Vec<u8> {
    stream(&format!("{dict} /Filter /FlateDecode"), &pdfcraft_cos::Stream::flate(pdfcraft_cos::Dict::new(), data).raw)
}

/// A file of 200 × 300 pages, each drawing its content with `resources`; `objects` are numbered
/// from 3, the pages and their contents after them.
fn file(contents: &[&str], resources: &str, objects: &[Vec<u8>]) -> Vec<u8> {
    let first = 3 + objects.len();
    let kids: Vec<String> = (0..contents.len()).map(|i| format!("{} 0 R", first + 2 * i)).collect();
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {} /MediaBox [0 0 200 300] >>", kids.join(" "), contents.len()).into_bytes(),
    ];
    bodies.extend(objects.iter().cloned());
    for (i, content) in contents.iter().enumerate() {
        bodies.push(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << {resources} >> >>", first + 2 * i + 1).into_bytes());
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

/// Compare `edited` with a blank page, with the work bounds and no clock; the result and how
/// long it took.
fn against_blank(edited: &[u8]) -> (Result<Inspection, InspectError>, Duration) {
    let blank = file(&["0 g"], "", &[]);
    let started = Instant::now();
    let result = inspect_with(&blank, edited, Limits { deadline: Duration::from_secs(3_600), ..Limits::default() });
    (result, started.elapsed())
}

/// An image the dictionary calls 100 × 100, drawn over the page.
fn drawn(image: Vec<u8>) -> Vec<u8> {
    file(&["q 200 0 0 300 0 0 cm /Im Do Q"], "/XObject << /Im 3 0 R >>", &[image])
}

#[test]
fn a_jpx_header_declaring_a_huge_image_ends_within_the_bounds() {
    // A JPEG 2000 codestream whose SIZ marker declares 50,000 × 50,000 samples: 54 bytes.
    let mut jpx = vec![0xFF, 0x4F, 0xFF, 0x51, 0, 41, 0, 0];
    for value in [50_000u32, 50_000, 0, 0, 50_000, 50_000, 0, 0] {
        jpx.extend_from_slice(&value.to_be_bytes());
    }
    jpx.extend_from_slice(&[0, 1, 7, 1, 1]);
    jpx.extend_from_slice(&[0xFF, 0x52, 0, 12, 0, 0, 0, 1, 0, 5, 4, 4, 0, 1]);
    jpx.extend_from_slice(&[0xFF, 0x5C, 0, 19, 0x40]);
    jpx.extend((0..16).map(|band| 8u8 << 3 | (band % 2)));
    jpx.extend_from_slice(&[0xFF, 0xD9]);
    let image = stream("/Type /XObject /Subtype /Image /Width 100 /Height 100 /Filter /JPXDecode", &jpx);
    let (result, took) = against_blank(&drawn(image));
    assert!(matches!(result, Err(InspectError::TooComplex | InspectError::Edited(_))), "{result:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
}

#[test]
fn a_jbig2_page_declaring_a_huge_page_stops_on_the_codec_budget() {
    // Embedded JBIG2: a page information segment of 16,000 × 16,000 pixels (under the 2^28
    // bitmap cap, so the work allowance is what stops it), then the end of the page: 41 bytes.
    let mut jbig2 = Vec::new();
    jbig2.extend_from_slice(&[0, 0, 0, 0, 48, 0, 1, 0, 0, 0, 19]);
    jbig2.extend_from_slice(&16_000u32.to_be_bytes());
    jbig2.extend_from_slice(&16_000u32.to_be_bytes());
    jbig2.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    jbig2.extend_from_slice(&[0, 0, 0, 1, 49, 0, 1, 0, 0, 0, 0]);
    let image =
        stream("/Type /XObject /Subtype /Image /Width 100 /Height 100 /BitsPerComponent 1 /ColorSpace /DeviceGray /Filter /JBIG2Decode", &jbig2);
    let (result, took) = against_blank(&drawn(image));
    assert!(matches!(result, Err(InspectError::TooComplex | InspectError::Edited(_))), "{result:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
}

#[test]
fn a_font_program_of_four_mib_is_read_within_the_bounds() {
    // Inter, padded to 4 MiB: about 200 KB compressed.
    let mut program = include_bytes!("../../../../assets/fonts/Inter-Regular.ttf").to_vec();
    program.resize(4 << 20, 0);
    let font_file = flate(&format!("/Length1 {}", program.len()), &program);
    let descriptor = b"<< /Type /FontDescriptor /FontName /Inter /Flags 32 /ItalicAngle 0 /Ascent 900 /Descent -200 /CapHeight 700 /StemV 80 \
        /FontBBox [0 -200 1000 900] /FontFile2 3 0 R >>"
        .to_vec();
    let widths = vec!["600"; 95].join(" ");
    let font = format!(
        "<< /Type /Font /Subtype /TrueType /BaseFont /Inter /FirstChar 32 /LastChar 126 /Widths [{widths}] /Encoding /WinAnsiEncoding /FontDescriptor 4 0 R >>"
    )
    .into_bytes();
    let edited = file(&["BT /F 24 Tf 20 150 Td (Hello) Tj ET"], "/Font << /F 5 0 R >>", &[font_file, descriptor, font]);
    assert!(edited.len() < 256 << 10, "{}", edited.len());
    let (result, took) = against_blank(&edited);
    // Read and drawn: the text is a change outside any redaction box.
    let inspection = result.unwrap();
    assert_eq!(inspection.text.pages_with_text_added, 1);
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1);
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// `levels` form XObjects, each drawing the next; the innermost draws an image.
fn nested_forms(levels: usize) -> Vec<u8> {
    let mut objects =
        vec![stream("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8", &[0, 80, 160, 240])];
    for level in 0..levels {
        let (name, next) = if level + 1 == levels { ("Im", 3) } else { ("F", 5 + level) };
        objects.push(stream(
            &format!("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Resources << /XObject << /{name} {next} 0 R >> >>"),
            format!("/{name} Do").as_bytes(),
        ));
    }
    // The page draws the outermost form (object 4) over the page.
    file(&["q 200 0 0 300 0 0 cm 0.005 0 0 0.00333 0 0 cm /F Do Q"], "/XObject << /F 4 0 R >>", &objects)
}

#[test]
fn forms_nested_to_the_depth_limit_are_read_and_one_more_is_too_complex() {
    let limit = Limits::default().form_depth;
    let (result, took) = against_blank(&nested_forms(limit));
    // Sixteen deep: read, drawn by hayro too, and an image added outside any box.
    let inspection = result.unwrap();
    assert_eq!(inspection.content.pages_with_images_added, 1, "{inspection:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    let (result, took) = against_blank(&nested_forms(limit + 1));
    assert_eq!(result, Err(InspectError::TooComplex));
    assert!(took < Duration::from_secs(10), "{took:?}");
}

#[test]
fn two_thousand_small_pages_with_an_image_each_are_compared_within_the_bounds() {
    // Every page draws the same small image and a word; the edited file is a rewrite (another
    // header), so every page's image placements are read from hayro on both sides.
    let image = stream("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8", &[0, 80, 160, 240]);
    let contents: Vec<String> = (0..MAX_PAGES).map(|i| format!("q 20 0 0 20 10 10 cm /Im Do Q BT /F 12 Tf 50 150 Td (Page {i}) Tj ET")).collect();
    let contents: Vec<&str> = contents.iter().map(String::as_str).collect();
    let resources = "/XObject << /Im 3 0 R >> /Font << /F << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >>";
    let base = file(&contents, resources, &[image]);
    let mut edited = base.clone();
    edited[7] = b'6';
    let started = Instant::now();
    let inspection = inspect_with(&base, &edited, Limits { deadline: Duration::from_secs(3_600), ..Limits::default() }).unwrap();
    let took = started.elapsed();
    assert!(!inspection.base_kept);
    assert_eq!(inspection.pages.before, MAX_PAGES as u32);
    assert_eq!(inspection.content, ContentChanges::default());
    assert!(took < Duration::from_secs(60), "{took:?}");
}

/// A page drawing form `/Fm` (object 3) whose dictionary has `coding` and whose data is `data`,
/// with `objects` from 4.
fn coded_form(coding: &str, data: &[u8], objects: &[Vec<u8>]) -> Vec<u8> {
    let mut all = vec![stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 200 300] {coding}"), data)];
    all.extend(objects.iter().cloned());
    file(&["/Fm Do"], "/XObject << /Fm 3 0 R >>", &all)
}

/// `data` under `layers` ASCIIHex layers, and the filter array that decodes it.
fn hex_layers(data: &[u8], layers: usize) -> (Vec<u8>, String) {
    let mut out = data.to_vec();
    for _ in 0..layers {
        let mut hex: Vec<u8> = out.iter().flat_map(|byte| format!("{byte:02X}").into_bytes()).collect();
        hex.push(b'>');
        out = hex;
    }
    (out, format!("[{}]", vec!["/ASCIIHexDecode"; layers].join(" ")))
}

#[test]
fn a_filter_chain_longer_than_eight_is_too_complex() {
    let content = b"0 g 10 10 20 20 re f";
    let (data, chain) = hex_layers(content, 8);
    let (result, _) = against_blank(&coded_form(&format!("/Filter {chain}"), &data, &[]));
    assert!(result.is_ok(), "{result:?}");
    let (data, chain) = hex_layers(content, 9);
    let (result, took) = against_blank(&coded_form(&format!("/Filter {chain}"), &data, &[]));
    assert_eq!(result, Err(InspectError::TooComplex));
    assert!(took < Duration::from_secs(5), "{took:?}");
    // The same chain given indirectly.
    let (result, _) = against_blank(&coded_form("/Filter 4 0 R", &data, &[chain.into_bytes()]));
    assert_eq!(result, Err(InspectError::TooComplex));
}

#[test]
fn coding_entries_that_refer_back_are_refused_quickly() {
    let content = b"0 g 10 10 20 20 re f";
    for (case, coding, extra) in [
        ("the stream itself", "/Filter 3 0 R", b"null".to_vec()),
        ("an array naming itself", "/Filter 4 0 R", b"[4 0 R]".to_vec()),
        ("parameters naming themselves", "/Filter /FlateDecode /DecodeParms 4 0 R", b"[4 0 R]".to_vec()),
    ] {
        let (result, took) = against_blank(&coded_form(coding, content, &[extra]));
        assert!(matches!(result, Err(InspectError::Edited(_))), "{case}: {result:?}");
        assert!(took < Duration::from_secs(5), "{case}: {took:?}");
    }
}

#[test]
fn a_chain_expanding_past_the_content_bound_is_too_complex() {
    // 2 MiB of white space (content that draws nothing), compressed twice to a few kilobytes,
    // under a 1 MiB bound for decoded content: every step of the chain counts.
    let spaces = vec![b' '; 2 << 20];
    let once = pdfcraft_cos::Stream::flate(pdfcraft_cos::Dict::new(), &spaces).raw;
    let twice = pdfcraft_cos::Stream::flate(pdfcraft_cos::Dict::new(), &once).raw;
    let edited = coded_form("/Filter [/FlateDecode /FlateDecode]", &twice, &[]);
    let blank = file(&["0 g"], "", &[]);
    let started = Instant::now();
    let limits = Limits { deadline: Duration::from_secs(3_600), content_bytes: 1 << 20, ..Limits::default() };
    assert_eq!(inspect_with(&blank, &edited, limits), Err(InspectError::TooComplex));
    assert!(started.elapsed() < Duration::from_secs(5));
    // Within the default bound it is read.
    assert!(against_blank(&edited).0.is_ok());
}
