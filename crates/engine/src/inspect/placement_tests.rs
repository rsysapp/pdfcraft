//! Embed build: image placements are where hayro's interpreter draws (Ruling R6b-18).
//!
//! Each case is a valid but unusual content stream. The conformance tests check that the image
//! uses the inspector reports are where the same interpreter's render puts ink; the end-to-end
//! tests check that the comparison follows them: an image changed where it shows is a change
//! outside the redaction boxes, and the same image unchanged, or redacted under a box, is not.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::structure::{ImageUse, unit_square};
use super::*;

// ── files ───────────────────────────────────────────────────────────────────────────────────

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// A 200 × 300 page: catalog 1, page tree 2, the page 3 drawing `content` (object 4) with
/// `resources` (a resource dictionary's entries, plus Helvetica as `/F1`), then `objects` from 5.
fn one_page(content: &[u8], resources: &str, objects: &[Vec<u8>]) -> Vec<u8> {
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> {resources} >> >>")
            .into_bytes(),
        stream("", content),
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

/// An uncompressed grey image XObject.
fn grey(width: usize, height: usize, pixels: &[u8]) -> Vec<u8> {
    stream(&format!("/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8"), pixels)
}

/// A form XObject drawing `content` with `/XObject` resources `xobjects`.
fn form(bbox: &str, matrix: &str, xobjects: &str, content: &str) -> Vec<u8> {
    stream(&format!("/Type /XObject /Subtype /Form /BBox [{bbox}] /Matrix [{matrix}] /Resources << /XObject << {xobjects} >> >>"), content.as_bytes())
}

/// The inline image whose data holds `EI` followed by content that reads as operators: one byte
/// per pixel, every byte printable (dark grey).
const INLINE_DATA: &[u8] = b"0 EI Q 2 0 0 2 0 0 cm 00";

fn inline_image() -> String {
    format!("BI /W {} /H 1 /BPC 8 /CS /G ID {} EI", INLINE_DATA.len(), String::from_utf8(INLINE_DATA.to_vec()).unwrap())
}

/// The cases: each a page drawing `first`, then `image` (an image XObject, object 5) where its
/// content stream says, then `last` (both in user space), returning the file.
type Case = (&'static str, fn(Vec<u8>, &str, &str) -> Vec<u8>);

fn more_q_than_q(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    // The first `Q` has no `q`: it restores the state the page started in, dropping the `cm`.
    let content = format!("{first}\n100 0 0 100 20 20 cm Q q 100 0 0 100 50 100 cm /Im1 Do Q\n{last}");
    one_page(content.as_bytes(), "/XObject << /Im1 5 0 R >>", &[image])
}

fn cm_with_odd_operands(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    // A `cm` takes its last six operands: a name among them makes it no `cm` at all; an extra
    // number before six is ignored.
    let content = format!("{first}\n2 0 0 2 0 0 /Junk cm 7 1 0 0 1 0 0 cm q 100 0 0 100 50 100 cm /Im1 Do Q\n{last}");
    one_page(content.as_bytes(), "/XObject << /Im1 5 0 R >>", &[image])
}

fn cm_inside_text(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    let content = format!("{first}\nq BT /F1 12 Tf 100 0 0 100 50 100 cm ET /Im1 Do Q\n{last}");
    one_page(content.as_bytes(), "/XObject << /Im1 5 0 R >>", &[image])
}

fn inline_image_holding_ei(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    // The text after `Q` decides nothing here (see `WORD`).
    let content = format!("{first}\nq 20 0 0 10 10 10 cm {} Q q 100 0 0 100 50 100 cm /Im1 Do Q\n{last}", inline_image());
    one_page(content.as_bytes(), "/XObject << /Im1 5 0 R >>", &[image])
}

fn form_clipped_by_bbox(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    // In form space the image covers 0–100 × 0–100 and the box keeps 0–50 × 0–50. The matrix
    // doubles x and moves the form to (10, 20); its last number is an indirect object.
    let fm = form("0 0 50 50", "2 0 0 1 10 7 0 R", "/Im 5 0 R", "100 0 0 100 0 0 cm /Im Do");
    let content = format!("{first}\n/Fm Do\n{last}");
    one_page(content.as_bytes(), "/XObject << /Fm 6 0 R >>", &[image, fm, b"20".to_vec()])
}

fn image_in_nested_form(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    // The outer form moves by (120, 20), its x an indirect object: the image covers
    // 130–230 × 30–130, and the inner form's box keeps 130–170 × 30–90 of it.
    let outer = form("0 0 100 100", "1 0 0 1 8 0 R 20", "/In 7 0 R", "/In Do");
    let inner = form("0 0 40 60", "1 0 0 1 10 10", "/Im 5 0 R", "100 0 0 100 0 0 cm /Im Do");
    let content = format!("{first}\n/Out Do\n{last}");
    one_page(content.as_bytes(), "/XObject << /Out 6 0 R >>", &[image, outer, inner, b"120".to_vec()])
}

fn image_in_soft_mask(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    // Black paint through a luminosity mask whose group draws the image at 50–150 × 100–200:
    // the page shows the image's light parts as ink (the group's backdrop, black, masks the rest).
    let group = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /CS /DeviceGray >> /Resources << /XObject << /M 5 0 R >> >>",
        b"q 100 0 0 100 50 100 cm /M Do Q",
    );
    let content = format!("{first}\nq /GS1 gs 0 g 0 0 200 300 re f Q\n{last}");
    one_page(content.as_bytes(), "/ExtGState << /GS1 << /SMask << /S /Luminosity /G 6 0 R >> >> >>", &[image, group])
}

const CASES: [Case; 7] = [
    ("more Q than q", more_q_than_q),
    ("cm with extra or non-numeric operands", cm_with_odd_operands),
    ("cm inside a text object", cm_inside_text),
    ("an inline image whose data holds EI", inline_image_holding_ei),
    ("a form with /Matrix whose /BBox clips", form_clipped_by_bbox),
    ("an image in a nested form", image_in_nested_form),
    ("an image in a soft mask", image_in_soft_mask),
];

/// Cases whose image a form's `/BBox` clips: the ink covers part of the placed image only.
fn clipped(case: &str) -> bool {
    matches!(case, "a form with /Matrix whose /BBox clips" | "an image in a nested form")
}

/// Run `check` on every case and fail with every case that failed, not only the first.
fn for_each_case(cases: &[Case], check: impl Fn(&str, fn(Vec<u8>, &str, &str) -> Vec<u8>)) {
    let failed: Vec<String> = cases
        .iter()
        .filter_map(|(case, build)| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(case, *build))).err().map(|panic| {
                let message = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()));
                format!("{case}: {}", message.unwrap_or_default())
            })
        })
        .collect();
    assert!(failed.is_empty(), "{} case(s) failed:\n{}", failed.len(), failed.join("\n"));
}

// ── conformance: the reported uses are where the render puts ink ────────────────────────────

/// The image uses the inspector reports for page 1 of `file`, and the page rendered upright.
fn reported(file: &[u8]) -> (Vec<ImageUse>, UprightRaster) {
    let mut budget = Budget::new(Limits::default());
    let side = read_side(file, None, true, &mut budget).map_err(|fail| format!("{fail:?}")).unwrap();
    let uses = side.structure.pages[0].image_uses.clone();
    let raster = side.hayro.render_upright(0, 600).unwrap();
    (uses, raster)
}

/// Every inked pixel of the render lies in a reported image (within a point), and every
/// reported image is inked: out to its sides (within a point and a half) unless `clipped`.
fn assert_drawn_where_reported(case: &str, file: &[u8], expected_uses: usize, clipped: bool) {
    let (uses, raster) = reported(file);
    let squares: Vec<[f64; 4]> = uses.iter().map(|used| unit_square(&used.matrix)).collect();
    assert_eq!(squares.len(), expected_uses, "{case}: {uses:?}");
    let scale = f64::from(raster.scale);
    let to_user = |px: usize, py: usize| {
        (f64::from(raster.crop[0]) + (px as f64 + 0.5) / scale, f64::from(raster.crop[1]) + f64::from(raster.height_pt) - (py as f64 + 0.5) / scale)
    };
    let mut ink: Vec<[f64; 4]> = vec![[f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY]; squares.len()];
    for py in 0..raster.height as usize {
        for px in 0..raster.width as usize {
            if raster.rgba.get((py * raster.width as usize + px) * 4).is_none_or(|r| *r >= 128) {
                continue;
            }
            let (x, y) = to_user(px, py);
            let mut found = false;
            for (b, reach) in squares.iter().zip(ink.iter_mut()) {
                if x >= b[0] - 1.0 && x <= b[2] + 1.0 && y >= b[1] - 1.0 && y <= b[3] + 1.0 {
                    found = true;
                    *reach = [reach[0].min(x), reach[1].min(y), reach[2].max(x), reach[3].max(y)];
                }
            }
            assert!(found, "ink at {x:.1},{y:.1} outside every reported image {squares:?}");
        }
    }
    for (b, reach) in squares.iter().zip(&ink) {
        assert!(reach[0] <= reach[2], "no ink where an image is reported at {b:?} (all reported: {squares:?})");
        if !clipped {
            assert!(
                (0..4).all(|i| (reach[i] - b[i]).abs() <= 1.5),
                "the image reported at {b:?} is drawn over {reach:?} (all reported: {squares:?})"
            );
        }
    }
}

/// A black image, 4 × 4 pixels.
fn black() -> Vec<u8> {
    grey(4, 4, &[0; 16])
}

#[test]
fn image_placements_are_where_the_interpreter_draws() {
    for_each_case(&CASES, |case, build| {
        let (image, uses) = match case {
            // The mask shows black paint where its image is light.
            "an image in a soft mask" => (grey(4, 4, &[255; 16]), 1),
            "an inline image whose data holds EI" => (black(), 2),
            _ => (black(), 1),
        };
        assert_drawn_where_reported(case, &build(image, "", ""), uses, clipped(case));
    });
}

#[test]
fn an_image_after_an_operator_that_ends_the_content_is_refused_not_left_unplaced() {
    // `m` with one operand: hayro stops reading the content stream there (a malformed path
    // operator could otherwise draw anything), so the image after it is never drawn. Content
    // read by nothing is an error, not an image without a place.
    let file = one_page(b"q 100 0 0 100 50 100 cm 10 m /Im1 Do Q", "/XObject << /Im1 5 0 R >>", &[black()]);
    let mut budget = Budget::new(Limits::default());
    match read_side(&file, None, true, &mut budget) {
        Err(Fail::Read(message)) => assert!(message.contains("stopped reading"), "{message}"),
        Err(other) => panic!("expected a read failure, got {other:?}"),
        Ok(_) => panic!("expected a read failure"),
    }
}

// ── end to end: the comparison follows the uses ─────────────────────────────────────────────

/// A `width` × `height` grey texture, as a scan has.
fn texture(width: usize, height: usize) -> Vec<u8> {
    (0..width * height).map(|i| (96 + (i % width) * 7 % 61 + (i / width) * 3 % 37) as u8).collect()
}

/// A 100 × 100 image's `pixels` with columns `x0..x1` × rows `y0..y1` (row 0 at the top) changed.
fn with_change(pixels: &[u8], (x0, x1): (usize, usize), (y0, y1): (usize, usize), change: impl Fn(u8) -> u8) -> Vec<u8> {
    let mut out = pixels.to_vec();
    for y in y0..y1 {
        for x in x0..x1 {
            out[y * 100 + x] = change(out[y * 100 + x]);
        }
    }
    out
}

/// A word the edited files redact, with the box that covers it. The word comes before the
/// images: after an inline image, a text operator within twenty operators makes hayro take the
/// first `EI` in the data for the image's end.
const WORD: &str = "BT /F1 12 Tf 150 20 Td (X) Tj ET";
const WORD_BOX: &str = "0 g 146 14 16 18 re f";

fn outside_redactions(base: &[u8], edited: &[u8]) -> u32 {
    let inspection = inspect(base, edited, InspectLimits::default()).unwrap();
    assert!(!inspection.base_kept);
    inspection.content.pages_changed_outside_redactions
}

/// Each case with a 100 × 100 texture, rewritten with a word redacted elsewhere: the image
/// changed by 6 levels where it shows (too little for the page render to notice) is a change
/// outside the redaction boxes; unchanged it is not.
#[test]
fn an_image_changed_outside_the_boxes_is_a_change_outside_redactions() {
    for_each_case(&CASES, |case, build| {
        let original = texture(100, 100);
        let base = build(grey(100, 100, &original), WORD, "");
        // Columns 10–30, rows 60–90: shown in every case (in the soft mask, its light parts let
        // the black paint through).
        let changed = with_change(&original, (10, 30), (60, 90), |v| v + 6);
        assert_eq!(outside_redactions(&base, &build(grey(100, 100, &changed), "", WORD_BOX)), 1, "{case}: changed");
        assert_eq!(outside_redactions(&base, &build(grey(100, 100, &original), "", WORD_BOX)), 0, "{case}: unchanged");
    });
}

/// Where hayro draws the image is where its pixels are checked: an image redacted under a box
/// drawn over it there is accepted, and the same with pixels far from the box changed is not.
#[test]
fn an_image_redacted_under_a_box_where_it_is_drawn_is_accepted() {
    // Columns 10–30 and rows 10–30 of an image over 50–150 × 100–200 (a pixel a point) are
    // 60–80 × 170–190. (In the soft mask, cleared pixels let no paint through: the box is drawn
    // black over them.)
    let at_50_100 = ((10, 30), (10, 30), "0 g 60 170 20 20 re f");
    // Columns 10–20, rows 70–90 of the form's image (two points a pixel across) are 30–50 × 30–50.
    let in_form = ((10, 20), (70, 90), "0 g 30 30 20 20 re f");
    // Columns 10–30, rows 60–90 of the nested form's image are 140–160 × 40–70.
    let in_nested = ((10, 30), (60, 90), "0 g 140 40 20 30 re f");
    let placed = [
        (CASES[0], at_50_100),
        (CASES[1], at_50_100),
        (CASES[2], at_50_100),
        (CASES[3], at_50_100),
        (CASES[4], in_form),
        (CASES[5], in_nested),
        (CASES[6], at_50_100),
    ];
    let cases: Vec<Case> = placed.iter().map(|(case, _)| *case).collect();
    for_each_case(&cases, |case, build| {
        let Some((_, (columns, rows, cover))) = placed.iter().find(|((name, _), _)| *name == case) else { panic!("{case}") };
        let original = texture(100, 100);
        let base = build(grey(100, 100, &original), WORD, "");
        let redacted = with_change(&original, *columns, *rows, |_| 0);
        let boxes = format!("{WORD_BOX}\n{cover}");
        assert_eq!(outside_redactions(&base, &build(grey(100, 100, &redacted), "", &boxes)), 0, "{case}: redacted");
        let forged = with_change(&redacted, (60, 80), (60, 80), |v| v + 6);
        assert_eq!(outside_redactions(&base, &build(grey(100, 100, &forged), "", &boxes)), 1, "{case}: forged");
    });
}

fn image_in_tiling_pattern(image: Vec<u8>, first: &str, last: &str) -> Vec<u8> {
    // The top half of the page is painted with a 50-point cell holding the image.
    let pattern = stream(
        "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 50 50] /XStep 50 /YStep 50 /Resources << /XObject << /Im 5 0 R >> >>",
        b"50 0 0 50 0 0 cm /Im Do",
    );
    let content = format!("{first}\nq /Pattern cs /P1 scn 0 150 200 150 re f Q\n{last}");
    one_page(content.as_bytes(), "/Pattern << /P1 6 0 R >>", &[image, pattern])
}

/// An image in a tiling pattern's cell is reported as such, and compared by identity: changed
/// at all (even by too little for the page render to notice), it is a change outside the
/// redaction boxes.
#[test]
fn an_image_in_a_tiling_pattern_is_compared_by_identity() {
    let original = texture(100, 100);
    let base = image_in_tiling_pattern(grey(100, 100, &original), WORD, "");
    let (uses, _) = reported(&base);
    assert_eq!(uses.len(), 1, "one use per paint that uses the pattern");
    assert!(uses.iter().all(|used| used.tiled), "{uses:?}");
    let changed = with_change(&original, (10, 30), (60, 90), |v| v + 6);
    assert_eq!(outside_redactions(&base, &image_in_tiling_pattern(grey(100, 100, &changed), "", WORD_BOX)), 1, "changed");
    assert_eq!(outside_redactions(&base, &image_in_tiling_pattern(grey(100, 100, &original), "", WORD_BOX)), 0, "unchanged");
}

/// A stencil mask is identified and placed like any other image (hayro hands it to the device
/// as a stencil, filled with the current colour).
#[test]
fn a_stencil_mask_is_placed_like_any_image() {
    let mask = stream("/Type /XObject /Subtype /Image /Width 8 /Height 8 /ImageMask true /BitsPerComponent 1", &[0; 8]);
    let file = one_page(b"0 g q 100 0 0 100 50 100 cm /Im1 Do Q", "/XObject << /Im1 5 0 R >>", &[mask]);
    assert_drawn_where_reported("a stencil mask", &file, 1, false);
    assert!(reported(&file).0[0].image.is_some());
}
