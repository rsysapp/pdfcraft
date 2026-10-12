//! Embed build: optional content is part of what a glyph, image or form is (R6b-29).
//!
//! hayro draws every layer, so the text, images and pixels of every layer are compared. Each
//! glyph, image and form also carries the groups it is inside and whether it shows by default:
//! content moved into or out of a layer, or a layer's content covering or uncovering what shows by
//! default, is a change outside the redactions. Optional content the inspector cannot evaluate
//! against the document's `/OCProperties` (as readers would differ on it) fails the inspection.
//!
//! The reviewer's cases (N1) each also apply one ordinary word redaction; on `fd434a2` they were
//! accepted with no change outside the redactions, while PDFium showed the clause or mark appear
//! or disappear.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// A 200 × 300 page drawing `content` (object 4) with Helvetica as `/F1`, `/L` (group 6) and
/// `/M` (membership dictionary 7, `membership`) as property lists, form `/Fm` (object 5,
/// `form`) as an XObject; the catalog has `catalog` entries (by default, group 6 off).
fn file(catalog: &str, content: &str, form: &[u8], membership: &str) -> Vec<u8> {
    let bodies: Vec<Vec<u8>> = vec![
        format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>").into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> \
          /Properties << /L 6 0 R /M 7 0 R >> /XObject << /Fm 5 0 R >> >> >>"
            .to_vec(),
        stream("", content.as_bytes()),
        form.to_vec(),
        b"<< /Type /OCG /Name (Layer) >>".to_vec(),
        membership.as_bytes().to_vec(),
    ];
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

const OFF: &str = "/OCProperties << /OCGs [6 0 R] /D << /OFF [6 0 R] >> >>";
const ON: &str = "/OCProperties << /OCGs [6 0 R] /D << >> >>";
const MEMBERSHIP: &str = "<< /Type /OCMD /OCGs [6 0 R] /P /AllOn >>";
/// A form drawing nothing (the files' default `/Fm`).
fn empty_form() -> Vec<u8> {
    stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300]", b"")
}

/// The clause, the word the edits redact, and the redaction box over it.
const CLAUSE: &str = "BT /F1 12 Tf 20 150 Td (Seller is liable) Tj ET";
const WORD: &str = "BT /F1 12 Tf 150 20 Td (X) Tj ET";
const BOX: &str = "0 g 146 14 16 18 re f";
/// A drawn mark (a signature's stroke).
const MARK: &str = "0 G 3 w 20 220 m 80 260 l 140 220 l S";

fn run(base: &[u8], edited: &[u8]) -> Result<Inspection, InspectError> {
    inspect(base, edited, InspectLimits::default())
}

fn outside(base: &[u8], edited: &[u8]) -> u32 {
    let inspection = run(base, edited).unwrap_or_else(|error| panic!("{error:?}"));
    inspection.content.pages_changed_outside_redactions
}

#[test]
fn content_moved_into_or_out_of_a_layer_that_is_off_is_outside_the_redactions() {
    let page = |catalog: &str, content: &str| file(catalog, content, &empty_form(), MEMBERSHIP);
    // (case, base content, edited content (with the redaction box))
    let cases = [
        ("a visible clause wrapped in the off layer", format!("{CLAUSE}\n{WORD}"), format!("/OC /L BDC {CLAUSE} EMC\n{BOX}")),
        ("the same through a membership dictionary", format!("{CLAUSE}\n{WORD}"), format!("/OC /M BDC {CLAUSE} EMC\n{BOX}")),
        ("a clause in the off layer moved out of it", format!("/OC /L BDC {CLAUSE} EMC\n{WORD}"), format!("{CLAUSE}\n{BOX}")),
        ("a drawn mark moved into the off layer", format!("{MARK}\n{CLAUSE}\n{WORD}"), format!("/OC /L BDC {MARK} EMC\n{CLAUSE}\n{BOX}")),
        (
            "a mark under a white cover in the off layer, removed",
            format!("{MARK}\n/OC /L BDC 1 g 10 200 150 80 re f EMC\n{CLAUSE}\n{WORD}"),
            format!("/OC /L BDC 1 g 10 200 150 80 re f EMC\n{CLAUSE}\n{BOX}"),
        ),
    ];
    for (case, base, edited) in cases {
        let base = page(OFF, &base);
        assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default(), "{case}");
        assert_eq!(outside(&base, &page(OFF, &edited)), 1, "{case}");
    }
    // Membership is identity whether the layer shows or not: the same with the layer on.
    let base = page(ON, &format!("{CLAUSE}\n{WORD}"));
    assert_eq!(outside(&base, &page(ON, &format!("/OC /L BDC {CLAUSE} EMC\n{BOX}"))), 1, "the layer on");
    // A redaction that leaves the layers as they were is a redaction like any other.
    let base = page(OFF, &format!("/OC /L BDC {MARK} EMC\n{CLAUSE}\n{WORD}"));
    let inspection = run(&base, &page(OFF, &format!("/OC /L BDC {MARK} EMC\n{CLAUSE}\n{BOX}"))).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.filled_areas.len()), (0, 1), "{inspection:?}");
}

#[test]
fn a_form_or_image_given_optional_content_is_outside_the_redactions() {
    let clause_form = |oc: &str| stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 200 300] {oc}"), CLAUSE.as_bytes());
    // The reviewer's case: the form drawing the clause given /OC (the layer, off).
    let base = file(OFF, &format!("/Fm Do\n{WORD}"), &clause_form(""), MEMBERSHIP);
    assert_eq!(outside(&base, &file(OFF, &format!("/Fm Do\n{BOX}"), &clause_form("/OC 6 0 R"), MEMBERSHIP)), 1);
    // Through the membership dictionary.
    assert_eq!(outside(&base, &file(OFF, &format!("/Fm Do\n{BOX}"), &clause_form("/OC 7 0 R"), MEMBERSHIP)), 1);
    // An image moved into the layer (a form drawn twice keeps its key; the image is compared by
    // its place and membership).
    let image = |oc: &str| {
        stream(&format!("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 {oc}"), &[0, 255, 255, 0])
    };
    let base = file(OFF, &format!("q 60 0 0 60 20 200 cm /Fm Do Q\n{WORD}"), &image(""), MEMBERSHIP);
    let edited = file(OFF, &format!("q 60 0 0 60 20 200 cm /Fm Do Q\n{BOX}"), &image("/OC 6 0 R"), MEMBERSHIP);
    assert_eq!(outside(&base, &edited), 1);
    let edited = file(OFF, &format!("/OC /L BDC q 60 0 0 60 20 200 cm /Fm Do Q EMC\n{BOX}"), &image(""), MEMBERSHIP);
    assert_eq!(outside(&base, &edited), 1);
}

#[test]
fn optional_content_that_cannot_be_evaluated_is_refused() {
    let refused = |base: &[u8], edited: &[u8]| match run(base, edited) {
        Err(InspectError::Edited(message)) => message,
        other => panic!("expected Err(Edited), got {other:?}"),
    };
    // The reviewer's case: no /OCProperties at all, the clause wrapped in a membership
    // dictionary whose visibility expression hides it (PDFium hides it; others show it).
    let base = file("", &format!("{CLAUSE}\n{WORD}"), &empty_form(), "<< /Type /OCMD /VE [/Not 6 0 R] >>");
    let edited = file("", &format!("/OC /M BDC {CLAUSE} EMC\n{BOX}"), &empty_form(), "<< /Type /OCMD /VE [/Not 6 0 R] >>");
    assert!(refused(&base, &edited).contains("without /OCProperties"));
    // Any group without /OCProperties.
    let edited = file("", &format!("/OC /L BDC {CLAUSE} EMC\n{BOX}"), &empty_form(), MEMBERSHIP);
    assert!(refused(&base, &edited).contains("without /OCProperties"));
    // (case, catalog, content, membership dictionary)
    let cases = [
        ("a group named by another tag", OFF, "/Span /L BDC", MEMBERSHIP),
        ("a group missing from /OCGs", "/OCProperties << /OCGs [] /D << /OFF [6 0 R] >> >>", "/OC /L BDC", MEMBERSHIP),
        (
            "automatic states",
            "/OCProperties << /OCGs [6 0 R] /D << /AS [<< /Event /View /OCGs [6 0 R] /Category [/View] >>] >> >>",
            "/OC /L BDC",
            MEMBERSHIP,
        ),
        ("a base state /Unchanged", "/OCProperties << /OCGs [6 0 R] /D << /BaseState /Unchanged >> >>", "/OC /L BDC", MEMBERSHIP),
        ("a group both on and off", "/OCProperties << /OCGs [6 0 R] /D << /ON [6 0 R] /OFF [6 0 R] >> >>", "/OC /L BDC", MEMBERSHIP),
        ("another intent", "/OCProperties << /OCGs [6 0 R] /D << /Intent /Design >> >>", "/OC /L BDC", MEMBERSHIP),
        ("a membership dictionary with no groups", OFF, "/OC /M BDC", "<< /Type /OCMD >>"),
        ("an unknown policy", OFF, "/OC /M BDC", "<< /Type /OCMD /OCGs [6 0 R] /P /SomeOn >>"),
        ("a malformed visibility expression", OFF, "/OC /M BDC", "<< /Type /OCMD /VE [/Not 6 0 R 6 0 R] >>"),
        ("an /OC tag naming no property list", OFF, "/OC /Nowhere BDC", MEMBERSHIP),
        ("an /OC property list written inline", OFF, "/OC << /Type /OCG /Name (Inline) >> BDC", MEMBERSHIP),
        ("a tag whose inline list names a group", OFF, "/Span << /OC 6 0 R >> BDC", MEMBERSHIP),
    ];
    for (case, catalog, begin, membership) in cases {
        let base = file(catalog, &format!("{CLAUSE}\n{WORD}"), &empty_form(), membership);
        let edited = file(catalog, &format!("{begin} {CLAUSE} EMC\n{BOX}"), &empty_form(), membership);
        assert!(refused(&base, &edited).contains("cannot be evaluated"), "{case}");
    }
    // A form or image whose /OC is written directly, or is not a dictionary.
    for oc in ["/OC << /Type /OCMD /OCGs [6 0 R] >>", "/OC /Off", "/OC 4 0 R"] {
        let form = stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 200 300] {oc}"), CLAUSE.as_bytes());
        let base = file(OFF, &format!("{CLAUSE}\n{WORD}"), &empty_form(), MEMBERSHIP);
        let edited = file(OFF, &format!("/Fm Do\n{BOX}"), &form, MEMBERSHIP);
        assert!(refused(&base, &edited).contains("cannot be evaluated"), "{oc}");
    }
    // Evaluated alike by every reader: a visibility expression, with /OCProperties.
    let expression = "<< /Type /OCMD /VE [/Or [/Not 6 0 R] 6 0 R] >>";
    let base = file(OFF, &format!("/OC /M BDC {CLAUSE} EMC\n{WORD}"), &empty_form(), expression);
    let inspection = run(&base, &file(OFF, &format!("/OC /M BDC {CLAUSE} EMC\n{BOX}"), &empty_form(), expression)).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.filled_areas.len()), (0, 1), "{inspection:?}");
}

#[test]
fn marked_content_unbalanced_around_optional_content_is_refused() {
    let form =
        |content: &str| stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Resources << /Properties << /L 6 0 R >> >>", content.as_bytes());
    let base = file(OFF, &format!("{CLAUSE}\n{WORD}"), &empty_form(), MEMBERSHIP);
    // (case, page content, the form's content)
    let cases = [
        ("a form opening the layer and leaving it open", "/Fm Do\nBT /F1 12 Tf 20 150 Td (Seller is liable) Tj ET EMC".to_string(), "/OC /L BDC"),
        ("a form closing the page's layer", format!("/OC /L BDC /Fm Do {CLAUSE}"), "EMC"),
        ("the page leaving the layer open", format!("/OC /L BDC {CLAUSE}"), ""),
        ("an EMC with nothing open, then the layer", format!("EMC /OC /L BDC {CLAUSE} EMC"), ""),
    ];
    for (case, content, inside) in cases {
        let edited = file(OFF, &format!("{content}\n{BOX}"), &form(inside), MEMBERSHIP);
        match run(&base, &edited) {
            Err(InspectError::Edited(message)) => assert!(message.contains("not balanced"), "{case}: {message}"),
            other => panic!("{case}: {other:?}"),
        }
    }
    // Unbalanced marked content with no optional content anywhere is not refused for that.
    let plain = file("", &format!("/Span BMC {CLAUSE}\n{WORD}"), &empty_form(), MEMBERSHIP);
    assert!(run(&plain, &plain).is_ok());
}

#[test]
fn what_a_layer_covers_is_compared_with_the_layer_on_and_off() {
    // hayro alone ignores a visibility expression (it would read this membership as "all of
    // the off group on": hidden); every other reader evaluates the expression ("not the off
    // group": shown), and so does the inspector's default view.
    let membership = "<< /Type /OCMD /OCGs [6 0 R] /P /AllOn /VE [/Not 6 0 R] >>";
    let cover = "/OC /M BDC 1 g 10 200 150 80 re f EMC";
    // A mark under that cover, removed: hidden by the cover by default, as in PDFium, but shown
    // with the layer off (a Layers panel): a change outside the redactions.
    let base = file(OFF, &format!("{MARK}\n{cover}\n{CLAUSE}\n{WORD}"), &empty_form(), membership);
    let edited = file(OFF, &format!("{cover}\n{CLAUSE}\n{BOX}"), &empty_form(), membership);
    assert_eq!(outside(&base, &edited), 1);
    // The cover in the off group instead: the mark showed by default and is gone.
    let hidden_cover = "/OC /L BDC 1 g 10 200 150 80 re f EMC";
    let base = file(OFF, &format!("{MARK}\n{hidden_cover}\n{CLAUSE}\n{WORD}"), &empty_form(), membership);
    let edited = file(OFF, &format!("{hidden_cover}\n{CLAUSE}\n{BOX}"), &empty_form(), membership);
    assert_eq!(outside(&base, &edited), 1);
    // A redaction elsewhere, the layered content as it was, is accepted in every view.
    let edited = file(OFF, &format!("{MARK}\n{hidden_cover}\n{CLAUSE}\n{BOX}"), &empty_form(), membership);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.filled_areas.len()), (0, 1), "{inspection:?}");
}

// ── R6b-36: every combination of a changed page's layers ────────────────────────────────────

/// Group 6 (`/L`, the layer the mark is in) and group 7 (`/M`, here a second group: the cover's
/// layer, `cover_group`), both listed, with `default` as the default configuration.
fn two_layers(default: &str, content: &str, cover_group: &str) -> Vec<u8> {
    file(&format!("/OCProperties << /OCGs [6 0 R 7 0 R] /D << {default} >> >>"), content, &empty_form(), cover_group)
}

const COVER_GROUP: &str = "<< /Type /OCG /Name (Cover) >>";
/// The cover's group with `/Usage /View /ViewState /OFF`: PDFium applies it before `/D`, and
/// hides the group by default whatever `/D` says.
const COVER_GROUP_USAGE: &str = "<< /Type /OCG /Name (Cover) /Usage << /View << /ViewState /OFF >> >> >>";
/// A white cover over the mark, in the cover's layer.
const COVER: &str = "/OC /M BDC 1 g 10 200 150 80 re f EMC";
/// The mark drawn another way.
const OTHER_MARK: &str = "0 G 3 w 20 220 m 80 250 l 140 230 l S";

/// The reviewer's C2 page: the mark in layer `/L`, the cover in `/M` over it, the clause, and
/// the word the edit redacts (or the box over it).
fn covered(mark: Option<&str>, redacted: bool) -> String {
    let mark = mark.map(|mark| format!("/OC /L BDC {mark} EMC\n")).unwrap_or_default();
    format!("{mark}{COVER}\n{CLAUSE}\n{}", if redacted { BOX } else { WORD })
}

#[test]
fn a_mark_under_a_cover_in_another_layer_is_compared_in_every_combination() {
    // R6b-36, the reviewer's C2 (`p_layer_combination`). With every layer on the cover hides the
    // mark; with every layer off neither is drawn. Only the layer of the mark on and the cover's
    // off shows the mark (a Layers panel's view): changed or removed there, which e91797d's three
    // views did not see, and accepted as one redaction.
    // (case, default configuration)
    for (case, default) in [("both groups on", ""), ("the mark's group off", "/OFF [6 0 R]"), ("the cover's group off", "/OFF [7 0 R]")] {
        let base = two_layers(default, &covered(Some(MARK), false), COVER_GROUP);
        assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default(), "{case}");
        let changed = two_layers(default, &covered(Some(OTHER_MARK), true), COVER_GROUP);
        assert_eq!(outside(&base, &changed), 1, "{case}: the mark changed");
        let removed = two_layers(default, &covered(None, true), COVER_GROUP);
        assert_eq!(outside(&base, &removed), 1, "{case}: the mark removed");
        // The layered content as it was, a redaction elsewhere: a redaction in every view.
        let redacted = two_layers(default, &covered(Some(MARK), true), COVER_GROUP);
        let inspection = run(&base, &redacted).unwrap();
        assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.filled_areas.len()), (0, 1), "{case}: {inspection:?}");
    }
}

#[test]
fn usage_on_a_group_a_changed_page_uses_is_refused() {
    // R6b-36, the reviewer's C2 with `/Usage /View /ViewState /OFF` on the cover's group
    // (`p_rbgroups_order_usage`): PDFium shows the mark by default, while `/D` says both groups
    // are on. Readers apply `/Usage` differently: a changed page that uses such a group fails.
    let refused = |base: &[u8], edited: &[u8]| match run(base, edited) {
        Err(InspectError::Edited(message)) => message,
        other => panic!("expected Err(Edited), got {other:?}"),
    };
    for (case, default) in [("plain", ""), ("radio-button groups and an order", "/RBGroups [[6 0 R 7 0 R]] /Order [6 0 R 7 0 R]")] {
        let base = two_layers(default, &covered(Some(MARK), false), COVER_GROUP_USAGE);
        // An unchanged page is compared as before.
        assert_eq!(run(&base, &base).unwrap().content, ContentChanges::default(), "{case}");
        for (what, mark) in [("the mark changed", Some(OTHER_MARK)), ("the mark removed", None), ("the mark kept", Some(MARK))] {
            let edited = two_layers(default, &covered(mark, true), COVER_GROUP_USAGE);
            assert!(refused(&base, &edited).contains("/Usage"), "{case}, {what}");
        }
    }
    // A group with `/Usage` that the changed page does not use: the page is compared as usual.
    let unused = "/OCProperties << /OCGs [6 0 R 7 0 R] /D << >> >>";
    let base = file(unused, &format!("/OC /L BDC {MARK} EMC\n{CLAUSE}\n{WORD}"), &empty_form(), COVER_GROUP_USAGE);
    let edited = file(unused, &format!("/OC /L BDC {MARK} EMC\n{CLAUSE}\n{BOX}"), &empty_form(), COVER_GROUP_USAGE);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.filled_areas.len()), (0, 1), "{inspection:?}");
}

/// A 200 × 300 page whose content draws, for each of `groups` groups (objects 5…), a small grey
/// square inside that group, then `content`.
fn many_layers(groups: usize, content: &str) -> Vec<u8> {
    let names: String = (0..groups).map(|i| format!("/G{i} {} 0 R ", 5 + i)).collect();
    let listed: String = (0..groups).map(|i| format!("{} 0 R ", 5 + i)).collect();
    let squares: String = (0..groups).map(|i| format!("/OC /G{i} BDC 0.5 g {} 260 8 8 re f EMC\n", 10 + 12 * i)).collect();
    let mut bodies: Vec<Vec<u8>> = vec![
        format!("<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [{listed}] /D << /OFF [5 0 R] >> >> >>").into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> /Properties << {names}>> >> >>"
        )
        .into_bytes(),
        stream("", format!("{squares}{content}").as_bytes()),
    ];
    bodies.extend((0..groups).map(|i| format!("<< /Type /OCG /Name (Layer {i}) >>").into_bytes()));
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

#[test]
fn a_changed_page_is_compared_in_up_to_sixty_four_views_of_its_layers() {
    // R6b-36: six groups are 64 combinations, each rendered and compared; seven are too many.
    let base = many_layers(6, &format!("{CLAUSE}\n{WORD}"));
    let inspection = run(&base, &many_layers(6, &format!("{CLAUSE}\n{BOX}"))).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.filled_areas.len()), (0, 1), "{inspection:?}");
    let base = many_layers(7, &format!("{CLAUSE}\n{WORD}"));
    assert_eq!(run(&base, &base).map(|inspection| inspection.content), Ok(ContentChanges::default()), "unchanged pages are not rendered");
    assert_eq!(run(&base, &many_layers(7, &format!("{CLAUSE}\n{BOX}"))), Err(InspectError::TooComplex));
}
