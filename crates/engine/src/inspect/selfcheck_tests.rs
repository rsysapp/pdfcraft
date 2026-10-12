//! Follow-up 8 (R6b-51; the redaction re-review's C1 and C2): a rewrite is the server's
//! redaction, checked against its base. Each forged rewrite below draws the honest black box
//! where "SECRETWORD" was and keeps the word somewhere no viewer draws it: in content-stream
//! tokens that draw nothing (C1), or in a resource drawn under the box whose data no comparison
//! reads (C2). Each must count its page outside its redactions (or fail), never pass as a
//! clean redaction.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
const LINE: &str = "BT /F1 12 Tf 20 150 Td (SECRETWORD) Tj ET";
const KEEP: &str = "BT /F1 12 Tf 20 250 Td (Keep) Tj ET";
const BLACK: &str = "0 g 16 144 96 22 re f";

/// A one-page file (200 × 300) drawing `content` with `resources` (Helvetica as /F1 unless
/// they name a /Font of their own); `extra` objects numbered from 5.
fn page(content: &str, resources: &str, extra: &[Vec<u8>]) -> Vec<u8> {
    let fonts = if resources.contains("/Font") { String::new() } else { format!("/Font << /F1 {HELVETICA} >>") };
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << {fonts} {resources} >> >>").into_bytes(),
        stream("", content.as_bytes()),
    ];
    bodies.extend(extra.iter().cloned());
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

/// Whether `edited` passes as a clean redaction of `base`: no page outside its redactions and a
/// redaction area found.
fn passes(base: &[u8], edited: &[u8]) -> bool {
    match inspect(base, edited, InspectLimits::default()) {
        Ok(inspection) => inspection.content.pages_changed_outside_redactions == 0 && !inspection.redaction_areas.is_empty(),
        Err(_) => false,
    }
}

#[test]
fn content_tokens_that_draw_nothing_cannot_keep_the_redacted_word() {
    let base = page(&format!("{LINE}\n{KEEP}"), "", &[]);
    assert!(passes(&base, &page(&format!("{BLACK}\n{KEEP}"), "", &[])), "the honest redaction");
    for (case, content) in [
        ("a comment", format!("% SECRETWORD\n{BLACK}\n{KEEP}")),
        ("an unknown operator in BX/EX", format!("BX (SECRETWORD) secretop EX\n{BLACK}\n{KEEP}")),
        ("an unknown operator", format!("(SECRETWORD) secretop\n{BLACK}\n{KEEP}")),
        ("an extra operand", format!("(SECRETWORD) 0 0 0 rg\n{BLACK}\n{KEEP}")),
        ("an operand inside a path", format!("q 0 g 16 144 96 22 re (SECRETWORD) f Q\n{KEEP}")),
        ("a tag", format!("/SECRETWORD BMC EMC\n{BLACK}\n{KEEP}")),
        ("a property name naming nothing", format!("/Span /SECRETWORD BDC EMC\n{BLACK}\n{KEEP}")),
        ("a marked point", format!("/Note << /Foo (SECRETWORD) >> DP\n{BLACK}\n{KEEP}")),
        ("a nested array in TJ", format!("{BLACK}\nBT /F1 12 Tf 20 250 Td [(Keep) [(SECRETWORD)]] TJ ET")),
        ("a dictionary in TJ", format!("{BLACK}\nBT /F1 12 Tf 20 250 Td [(Keep) << /X (SECRETWORD) >>] TJ ET")),
        ("an operand after Tf", format!("{BLACK}\nBT /F1 12 Tf /SECRETWORD 20 250 Td (Keep) Tj ET")),
        ("a trailing operand", format!("{BLACK}\n{KEEP}\n(SECRETWORD)")),
        ("a form naming nothing", format!("{BLACK}\n{KEEP}\n/SECRETWORD Do")),
        ("a graphics state naming nothing", format!("q /SECRETWORD gs Q\n{BLACK}\n{KEEP}")),
        ("a shading naming nothing", format!("q 0 0 0 0 re W n /SECRETWORD sh Q\n{BLACK}\n{KEEP}")),
    ] {
        assert!(!passes(&base, &page(&content, "", &[])), "{case}");
    }
    // A form the rewrite brings, the same.
    let form = stream("/Type /XObject /Subtype /Form /BBox [16 144 112 166]", b"% SECRETWORD\n0 g 16 144 96 22 re f");
    assert!(!passes(&base, &page(&format!("/Fm Do\n{KEEP}"), "/XObject << /Fm 5 0 R >>", &[form])), "a comment in a form");
}

#[test]
fn resources_drawn_under_the_box_cannot_keep_the_redacted_word() {
    let base = page(&format!("{LINE}\n{KEEP}"), "", &[]);
    // A tiling pattern whose cell shows the word, painted under the box.
    let pattern = stream(
        "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 200 300] /XStep 200 /YStep 300 /Resources << /Font << /F1 6 0 R >> >>",
        b"BT /F1 12 Tf 20 150 Td (SECRETWORD) Tj ET",
    );
    let painted = format!("q /Pattern cs /P1 scn 16 144 96 22 re f Q\n{BLACK}\n{KEEP}");
    assert!(!passes(&base, &page(&painted, "/Pattern << /P1 5 0 R >>", &[pattern, HELVETICA.as_bytes().to_vec()])), "a pattern's cell");
    // A shading whose function ends in the word, filled under the box.
    let shading = b"<< /ShadingType 1 /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec();
    let function = stream("/FunctionType 4 /Domain [0 1 0 1] /Range [0 1]", b"{ pop pop 0 } % SECRETWORD");
    let filled = format!("q 16 144 96 22 re W n /Sh1 sh Q\n{BLACK}\n{KEEP}");
    assert!(!passes(&base, &page(&filled, "/Shading << /Sh1 5 0 R >>", &[shading, function])), "a shading's function");
    // A colour profile holding the word, used under the box.
    let profile = stream("/N 1 /Alternate /DeviceGray", b"SECRETWORD not a profile");
    let coloured = format!("q /CS1 cs 0 sc 16 144 96 22 re f Q\n{BLACK}\n{KEEP}");
    assert!(!passes(&base, &page(&coloured, "/ColorSpace << /CS1 [/ICCBased 5 0 R] >>", &[profile])), "a colour profile");
    // The page's font changed in what it names, not in what it draws.
    for font in [
        "<< /Type /Font /Subtype /Type1 /BaseFont /SECRETWORD >>",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Differences [1 /S /E /C /R /E /T /W /O /R /D] >> >>",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 5 0 R >>",
    ] {
        let tounicode = stream("", b"% SECRETWORD\n/CIDInit /ProcSet findresource begin endcmap end");
        assert!(!passes(&base, &page(&format!("{BLACK}\n{KEEP}"), &format!("/Font << /F1 {font} >>"), &[tounicode])), "{font}");
    }
}
