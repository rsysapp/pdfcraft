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
        Ok(inspection) => inspection.content.pages_changed_outside_redactions == 0 && !inspection.filled_areas.is_empty(),
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

#[test]
fn every_operand_a_brought_stream_holds_is_what_its_operator_takes() {
    // Follow-up 8 audit (operators): operands of the wrong type, arrays and dictionaries
    // nested in operands, inline images, colour operands past what a colour takes, text in a
    // pattern's cell.
    let base = page(&format!("{LINE}\n{KEEP}\nq 10 0 0 10 150 10 cm BI /W 2 /H 1 /CS /G /BPC 8 ID\nab\nEI Q"), "", &[]);
    let kept_image = "q 10 0 0 10 150 10 cm BI /W 2 /H 1 /CS /G /BPC 8 ID\nab\nEI Q";
    assert!(passes(&base, &page(&format!("{BLACK}\n{KEEP}\n{kept_image}"), "", &[])), "the honest redaction");
    for (case, content) in [
        ("a string for a number", format!("0 0 (SECRETWORD) RG\n{BLACK}\n{KEEP}\n{kept_image}")),
        ("a string in a dash array", format!("[1 (SECRETWORD)] 0 d\n{BLACK}\n{KEEP}\n{kept_image}")),
        ("an array in a dash array", format!("[1 [2]] 0 d\n{BLACK}\n{KEEP}\n{kept_image}")),
        ("a string before a pattern name", format!("/Pattern cs (SECRETWORD) /P1 scn\n{BLACK}\n{KEEP}\n{kept_image}")),
        ("five colour operands", format!("1 2 3 4 5 sc\n{BLACK}\n{KEEP}\n{kept_image}")),
        (
            "an inline image's own entry",
            format!("{BLACK}\n{KEEP}\nq 10 0 0 10 150 10 cm BI /W 2 /H 1 /CS /G /BPC 8 /Secret (SECRETWORD) ID\nab\nEI Q"),
        ),
        ("an inline image the base had not", format!("{BLACK}\n{KEEP}\nq 10 0 0 10 150 10 cm BI /W 10 /H 1 /CS /G /BPC 8 ID\nSECRETWORD\nEI Q")),
        ("an unknown operator inside BX/EX", format!("BX /SECRETWORD secretop EX\n{BLACK}\n{KEEP}\n{kept_image}")),
    ] {
        assert!(!passes(&base, &page(&content, "", &[])), "{case}");
    }
    // A pattern whose cell the rewrite brings shows text no comparison reads.
    let cell = |content: &str| {
        stream(
            "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 200 300] /XStep 200 /YStep 300 /Resources << /Font << /F1 6 0 R >> >>",
            content.as_bytes(),
        )
    };
    let painted = |rest: &str| format!("q /Pattern cs /P1 scn 150 250 40 40 re f Q\n{rest}");
    let base = page(&painted(&format!("{LINE}\n{KEEP}")), "/Pattern << /P1 5 0 R >>", &[cell("0 g 0 0 10 10 re f"), HELVETICA.as_bytes().to_vec()]);
    let honest =
        page(&painted(&format!("{BLACK}\n{KEEP}")), "/Pattern << /P1 5 0 R >>", &[cell("0 g 0 0 10 10 re f"), HELVETICA.as_bytes().to_vec()]);
    assert!(passes(&base, &honest), "the pattern kept");
    let forged = page(
        &painted(&format!("{BLACK}\n{KEEP}")),
        "/Pattern << /P1 5 0 R >>",
        &[cell("0 g 0 0 10 10 re f BT /F1 1 Tf 20 150 Td (SECRETWORD) Tj ET"), HELVETICA.as_bytes().to_vec()],
    );
    assert!(!passes(&base, &forged), "text in a cell the rewrite brings");
}

#[test]
fn numbers_and_names_a_brought_stream_holds_are_the_base_s_or_the_redactor_s() {
    // Follow-up 8 audit (operators, covert channel): numbers carry data too. An operator a
    // rewrite brings is one the base's content had, operands and all, or one the redactor
    // writes, with numbers as it writes them.
    let base = page(&format!("{LINE}\n{KEEP}"), "", &[]);
    for (case, content) in [
        ("a line width of many digits", format!("0.83697982877 w\n{BLACK}\n{KEEP}")),
        ("a dash array of many numbers", format!("[83 69 67 82 69 84 87 79 82 68] 0 d\n{BLACK}\n{KEEP}")),
        ("a box of many digits", format!("0 g 16.8369798287 144 96 22 re f\n{KEEP}")),
        ("displacements in a row", format!("{BLACK}\nBT /F1 12 Tf 20 250 Td [(Keep) -83 -69 -67 -82] TJ ET")),
        ("a long run of path points", format!("{BLACK}\n{KEEP}\n{}", (0..64).map(|i| format!("{i} {i} l ")).collect::<String>())),
    ] {
        assert!(!passes(&base, &page(&content, "", &[])), "{case}");
    }
}

// ── P6-33: marks, overlays and what a redaction removes ─────────────────────────────────────

fn session_marks(base: &[u8], marks: Vec<crate::Shape>, apply: bool) -> Vec<u8> {
    let mut session = crate::Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    let id = session.open("base.pdf", None, std::sync::Arc::new(base.to_vec()), None).unwrap();
    for shape in marks {
        let style = crate::Style::default_for(&shape);
        session
            .apply(id, crate::Edit::AddAnnotation(crate::NewAnnotation { page: 0, style, shape, contents: String::new(), author: "Sari".into() }))
            .unwrap();
    }
    if apply {
        session.apply(id, crate::Edit::ApplyRedactions { pages: None }).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

fn mark(rect: [f64; 4], overlay: &str) -> crate::Shape {
    crate::Shape::Redact { quads: vec![crate::rect_quad(rect)], overlay: overlay.into(), look: Default::default() }
}

#[test]
fn an_update_reports_every_area_its_marks_mark_and_those_with_overlays() {
    // P6-33: the host checks each redaction area against the marks, and refuses marks that ask
    // for an overlay (the redactor fills every area black and ignores them).
    let base = page(&format!("{LINE}\n{KEEP}"), "", &[]);
    let marked = session_marks(&base, vec![mark([16.0, 144.0, 112.0, 166.0], ""), mark([16.0, 240.0, 60.0, 262.0], "Withheld")], false);
    let inspection = inspect(&base, &marked, InspectLimits::default()).unwrap();
    assert!(inspection.base_kept);
    assert_eq!(inspection.marked_areas, vec![(1, [16.0, 144.0, 112.0, 166.0]), (1, [16.0, 240.0, 60.0, 262.0])], "{inspection:?}");
    assert_eq!(inspection.marks_with_overlay, 1, "{inspection:?}");
    // A fill colour other than black, or repetition, is an overlay entry too.
    let red = "<< /Type /Annot /Subtype /Redact /Rect [16 144 112 166] /IC [1 0 0] >>";
    let with_red = page(&format!("{LINE}\n{KEEP}"), "", &[]).replace_annotations(red);
    assert_eq!(inspect(&base, &with_red, InspectLimits::default()).map(|found| found.marks_with_overlay), Ok(1));
}

trait WithAnnotations {
    fn replace_annotations(self, annotation: &str) -> Vec<u8>;
}

impl WithAnnotations for Vec<u8> {
    /// This one-page file with `annotation` (object 5) on its page, appended as an update.
    fn replace_annotations(self, annotation: &str) -> Vec<u8> {
        let page = format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 {HELVETICA} >> >> /Annots [5 0 R] >>");
        let at = self.windows(9).rposition(|w| w == b"startxref").unwrap();
        let prev: usize = std::str::from_utf8(&self[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        let mut out = self.clone();
        let mut rows = Vec::new();
        for (num, body) in [(3u32, page.as_str()), (5, annotation)] {
            rows.push((num, out.len()));
            out.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
        for (num, offset) in rows {
            out.extend_from_slice(format!("{num} 1\n{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        out
    }
}

#[test]
fn what_a_redaction_removes_under_a_mark_is_reported_apart() {
    // P6-33: annotations and form fields the redactor removes under a mark are counted as such,
    // never as annotations removed or form fields changed.
    let note = "<< /Type /Annot /Subtype /FreeText /Rect [20 148 100 162] /Contents (SECRETWORD) /DA (/Helv 12 Tf 0 g) >>";
    // Judged against the applied marks' areas (R6b-55): the marked file is what the output is
    // compared with.
    let base = page(&format!("{LINE}\n{KEEP}"), "", &[]).replace_annotations(note);
    let marked = session_marks(&base, vec![mark([16.0, 144.0, 112.0, 166.0], "")], false);
    let redacted = session_marks(&marked, Vec::new(), true);
    let inspection = inspect(&marked, &redacted, InspectLimits::default()).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!((inspection.annotations.removed, inspection.annotations.removed_by_redaction.total()), (0, 1), "{inspection:?}");
    // A form field whose widget lies under the mark.
    let field = page(&format!("{LINE}\n{KEEP}"), "", &[]);
    let with_field = {
        let widget = "<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /V (SECRETWORD) /Rect [20 146 100 162] /P 3 0 R /F 4 >>";
        let page_body = format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 {HELVETICA} >> >> /Annots [5 0 R] >>");
        let catalog = "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [5 0 R] >> >>";
        let at = field.windows(9).rposition(|w| w == b"startxref").unwrap();
        let prev: usize = std::str::from_utf8(&field[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        let mut out = field.clone();
        let mut rows = Vec::new();
        for (num, body) in [(1u32, catalog), (3, page_body.as_str()), (5, widget)] {
            rows.push((num, out.len()));
            out.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
        for (num, offset) in rows {
            out.extend_from_slice(format!("{num} 1\n{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        out
    };
    let marked = session_marks(&with_field, vec![mark([16.0, 144.0, 112.0, 166.0], "")], false);
    let redacted = session_marks(&marked, Vec::new(), true);
    let inspection = inspect(&marked, &redacted, InspectLimits::default()).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
    assert_eq!((inspection.form_fields_changed, inspection.form_fields_removed_by_redaction.total()), (0, 1), "{inspection:?}");
}

#[test]
fn an_update_carries_no_operator_channel_and_a_rewrite_draws_only_with_the_base_s_resources() {
    // Follow-up 8 audit (operators): in an appended update a page's content must stay byte for
    // byte the base's (R6b-3): one written again, even only with a comment, is a change the
    // host refuses.
    let base = page(&format!("{LINE}\n{KEEP}"), "", &[]);
    let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
    let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let mut update = base.clone();
    let offset = update.len();
    update.extend_from_slice(b"4 0 obj\n");
    update.extend_from_slice(&stream("", format!("% SECRETWORD\n{LINE}\n{KEEP}").as_bytes()));
    update.extend_from_slice(b"\nendobj\n");
    let xref = update.len();
    update.extend_from_slice(
        format!(
            "xref\n0 1\n0000000000 65535 f \n4 1\n{offset:010} 00000 n \ntrailer\n<< /Size 5 /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .as_bytes(),
    );
    let inspection = inspect(&base, &update, InspectLimits::default()).unwrap();
    assert_eq!(inspection.content.pages_with_content_changed, 1, "{inspection:?}");
    // In a rewrite, an operator naming a graphics state or shading the base page did not draw
    // with counts the page outside its redactions.
    for (case, content, resources, extra) in [
        ("a new graphics state", format!("q /GS9 gs Q\n{BLACK}\n{KEEP}"), "/ExtGState << /GS9 << /Type /ExtGState /CA 1 >> >>", vec![]),
        (
            "a new shading",
            format!("q 0 0 0 0 re W n /Sh9 sh Q\n{BLACK}\n{KEEP}"),
            "/Shading << /Sh9 << /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 1 0] /Function << /FunctionType 2 /Domain [0 1] /N 1 >> >> >>",
            vec![],
        ),
    ] {
        assert!(!passes(&base, &page(&content, resources, &extra)), "{case}");
    }
}
