//! Embed build: pages paired by what they show, and destinations that follow their pages.
//!
//! - R6b-31: changed pages pair by content similarity (the glyphs and images they share, at their
//!   places), position deciding only between pages equally alike.
//! - R6b-32: named destinations (`/Dests`, `/Names /Dests`) and links' destinations follow
//!   their pages as outline destinations do, reported in [`DestinationChanges`]; any other
//!   change fails. `render_risk` is checked on the base page too.
//! - The structure audit: a name tree viewers could resolve differently, an outline that loops,
//!   and page attributes the two readers inherit differently are refused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};

// ── files ───────────────────────────────────────────────────────────────────────────────────

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// A file of `objects` (number → body), with a table listing them (others free).
fn assemble(objects: &BTreeMap<u32, Vec<u8>>) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = BTreeMap::new();
    for (num, body) in objects {
        offsets.insert(*num, out.len());
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let size = objects.keys().max().copied().unwrap_or(0) + 1;
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for num in 1..size {
        match offsets.get(&num) {
            Some(offset) => out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes()),
            None => out.extend_from_slice(b"0000000000 00001 f \n"),
        }
    }
    out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

/// Append one update holding `objects` (number, body) to `base`, with `/Size` `size`.
fn update_objects(base: &[u8], objects: &[(u32, &str)], size: u32) -> Vec<u8> {
    let at = base.windows(9).rposition(|w| w == b"startxref").unwrap();
    let prev: usize = std::str::from_utf8(&base[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let mut out = base.to_vec();
    let mut rows = Vec::new();
    for (num, body) in objects {
        rows.push((*num, out.len()));
        out.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
    }
    rows.sort_unstable();
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (num, offset) in rows {
        out.extend_from_slice(format!("{num} 1\n{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    out
}

const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

/// `n` pages (200 × 300) showing "Page 1" … "Page n" in 24-point Helvetica at (20, 150), as the
/// fixture does: page `i` (from 0) is object 10 + i, its content 20 + i, its annotations
/// `annots(i)`. The catalog (1) has `catalog` entries; with `outline`, an outline (3) has an
/// entry per page (30 + i) titled "Page i+1" going there. `extra` objects are added as given.
fn pages(n: usize, catalog: &str, outline: bool, annots: impl Fn(usize) -> String, extra: &[(u32, String)]) -> Vec<u8> {
    pages_drawing(n, catalog, outline, annots, extra, |i| format!("BT /F1 24 Tf 20 150 Td (Page {}) Tj ET", i + 1))
}

/// [`pages`], page `i` drawing `content(i)`.
fn pages_drawing(
    n: usize,
    catalog: &str,
    outline: bool,
    annots: impl Fn(usize) -> String,
    extra: &[(u32, String)],
    content: impl Fn(usize) -> String,
) -> Vec<u8> {
    let mut objects: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let outlines = if outline { "/Outlines 3 0 R" } else { "" };
    objects.insert(1, format!("<< /Type /Catalog /Pages 2 0 R {outlines} {catalog} >>").into_bytes());
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 10 + i)).collect();
    objects.insert(2, format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 200 300] >>", kids.join(" ")).into_bytes());
    for i in 0..n {
        let page = 10 + i as u32;
        objects.insert(
            page,
            format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 {HELVETICA} >> >> {} >>", 20 + i, annots(i))
                .into_bytes(),
        );
        objects.insert(20 + i as u32, stream("", content(i).as_bytes()));
        if outline {
            let links = match (i, n - 1 - i) {
                (0, 0) => "".to_string(),
                (0, _) => format!("/Next {} 0 R", 31 + i),
                (_, 0) => format!("/Prev {} 0 R", 29 + i),
                _ => format!("/Prev {} 0 R /Next {} 0 R", 29 + i, 31 + i),
            };
            objects.insert(30 + i as u32, format!("<< /Title (Page {}) /Parent 3 0 R {links} /Dest [{page} 0 R /Fit] >>", i + 1).into_bytes());
        }
    }
    if outline {
        objects.insert(3, format!("<< /Type /Outlines /First 30 0 R /Last {} 0 R /Count {n} >>", 29 + n).into_bytes());
    }
    for (num, body) in extra {
        objects.insert(*num, body.clone().into_bytes());
    }
    assemble(&objects)
}

fn run(base: &[u8], edited: &[u8]) -> Result<Inspection, InspectError> {
    inspect(base, edited, InspectLimits::default())
}

fn refused(result: Result<Inspection, InspectError>) -> String {
    match result {
        Err(InspectError::Edited(message)) => message,
        other => panic!("expected Err(Edited), got {other:?}"),
    }
}

fn session(base: &[u8], edits: Vec<Edit>) -> Vec<u8> {
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    let id = session.open("base.pdf", None, Arc::new(base.to_vec()), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

fn annotation(page: usize, shape: Shape) -> Edit {
    Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: String::new(), author: "Sari".into() })
}

/// Redact the line "Page n" of page `page` whole, with room above and below, and apply it (a
/// box over the number alone, or a tight one, is not a redaction the redactor makes: it removes
/// every glyph whose font box the box touches, R6b-8), so every redacted page shows the same:
/// nothing.
fn redact_number(page: usize) -> Vec<Edit> {
    let mark = Shape::Redact { quads: vec![rect_quad([16.0, 120.0, 100.0, 180.0])], overlay: String::new(), look: Default::default() };
    vec![annotation(page, mark), Edit::ApplyRedactions { pages: None }]
}

fn sticky_note(page: usize) -> Edit {
    annotation(page, Shape::Note { at: [150.0, 20.0], icon: Default::default() })
}

// ── R6b-31: pages pair by what they show ────────────────────────────────────────────────────

#[test]
fn a_redaction_and_an_earlier_page_deleted_pair_by_content() {
    // The reviewer's case (I3): four pages "Page 1" … "Page 4" and an outline. What is left of
    // the redacted page could be a redaction of any page, so the first leftover page (an
    // earlier, deleted one) was taken for it, and the outline then looked wrong. Equally alike,
    // the pages pair as the destinations went, then by position.
    let base = pages(4, "", true, |_| String::new(), &[]);
    // (case, page redacted, page deleted, outline destinations that followed their page)
    for (case, redacted, deleted, destinations) in [("redact 4, delete 1", 3, 0, 4), ("redact 3, delete 2", 2, 1, 3), ("redact 2, delete 4", 1, 3, 1)]
    {
        let mut edits = redact_number(redacted);
        edits.push(Edit::DeletePages { pages: vec![deleted] });
        let inspection = run(&base, &session(&base, edits)).unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(inspection.pages, PageChanges { before: 4, after: 3, deleted: 1, ..PageChanges::default() }, "{case}: {inspection:?}");
        assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{case}: {inspection:?}");
        assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{case}: {inspection:?}");
        assert_eq!(inspection.redaction_areas.len(), 1, "{case}: {inspection:?}");
        assert_eq!(inspection.outlines, OutlineChanges { destinations_changed: destinations, ..OutlineChanges::default() }, "{case}: {inspection:?}");
    }
    // Without an outline nothing tells them apart: the redaction leaves page 4 showing what a
    // redaction of page 1 would show (a box over its only line), so which was deleted is a
    // guess. Position alone paired them until R6b-38; their renders now say they could each be
    // the page left, and the save fails.
    let plain = pages(4, "", false, |_| String::new(), &[]);
    let mut edits = redact_number(3);
    edits.push(Edit::DeletePages { pages: vec![0] });
    assert!(refused(run(&plain, &session(&plain, edits))).contains("cannot be told apart"));
    // A redaction that leaves the page's own line tells it apart: paired with its base page.
    let mut edits =
        vec![annotation(3, Shape::Redact { quads: vec![rect_quad([16.0, 40.0, 100.0, 100.0])], overlay: String::new(), look: Default::default() })];
    edits.extend([Edit::ApplyRedactions { pages: None }, Edit::DeletePages { pages: vec![0] }]);
    let inspection = run(&plain, &session(&plain, edits)).unwrap();
    assert_eq!((inspection.pages.deleted, inspection.content.pages_changed_outside_redactions), (1, 0), "{inspection:?}");
}

#[test]
fn a_changed_page_pairs_with_the_base_page_it_shares_most_with() {
    // Two pages whose text differs; a rewrite deletes the first and redacts a word of the
    // second. The second pairs with what is left of it, though the first comes first.
    let line = |y: u32, words: &str| format!("BT /F1 12 Tf 20 {y} Td ({words}) Tj ET");
    let two = |first: &str, second: &str| {
        let mut objects: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        objects.insert(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        objects.insert(2, b"<< /Type /Pages /Kids [10 0 R 11 0 R] /Count 2 /MediaBox [0 0 200 300] >>".to_vec());
        for (i, content) in [first, second].iter().enumerate() {
            objects.insert(
                10 + i as u32,
                format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 {HELVETICA} >> >> >>", 20 + i).into_bytes(),
            );
            objects.insert(20 + i as u32, stream("", content.as_bytes()));
        }
        assemble(&objects)
    };
    let base = two(
        &format!("{}\n{}", line(150, "Buyer pays"), line(120, "the price")),
        &format!("{}\n{}", line(150, "Seller ships"), line(120, "the goods")),
    );
    // The whole first line: the redactor re-sets what it keeps of a line (R6b-8).
    let mark = Shape::Redact { quads: vec![rect_quad([16.0, 144.0, 130.0, 163.0])], overlay: String::new(), look: Default::default() };
    let edited = session(&base, vec![annotation(1, mark), Edit::ApplyRedactions { pages: None }, Edit::DeletePages { pages: vec![0] }]);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.pages, PageChanges { before: 2, after: 1, deleted: 1, ..PageChanges::default() }, "{inspection:?}");
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{inspection:?}");
}

// ── scan item: a pairing that is not certain is refused ─────────────────────────────────────

/// "Page n", and on page `redacted` a box over "e n" instead (by hand, as a redactor that
/// removes only what its box covers would write it).
fn numbered(redacted: usize) -> impl Fn(usize) -> String {
    move |i| {
        if i == redacted {
            "BT /F1 24 Tf 20 150 Td (Pag) Tj ET\n0 g 63 140 37 35 re f".to_string()
        } else {
            format!("BT /F1 24 Tf 20 150 Td (Page {}) Tj ET", i + 1)
        }
    }
}

#[test]
fn pages_that_cannot_be_told_apart_are_refused_unless_a_destination_says() {
    // Four pages; page 4's number redacted and page 1 deleted. What is left of page 4 ("Pag")
    // could as well be what is left of page 1: on fd434a2 the first page that could be its
    // redaction (page 1) was taken.
    let plain = pages(4, "", false, |_| String::new(), &[]);
    let edited = session(&pages_drawing(4, "", false, |_| String::new(), &[], numbered(3)), vec![Edit::DeletePages { pages: vec![0] }]);
    assert!(refused(run(&plain, &edited)).contains("cannot be told apart"));
    // With an outline whose entry for page 4 now goes to the third page (and page 1's entry
    // to none), the destinations say which: accepted, and checked against that pairing.
    let base = pages(4, "", true, |_| String::new(), &[]);
    let edited = session(&pages_drawing(4, "", true, |_| String::new(), &[], numbered(3)), vec![Edit::DeletePages { pages: vec![0] }]);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.pages, PageChanges { before: 4, after: 3, deleted: 1, ..PageChanges::default() }, "{inspection:?}");
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.redaction_areas.len()), (0, 1), "{inspection:?}");
    assert_eq!(inspection.redaction_areas.first().map(|area| area.0), Some(3), "{inspection:?}");
    assert_eq!(inspection.outlines.destinations_changed, 4, "{inspection:?}");
}

/// Two pages: "Terms" and `first`, then "Terms", `second` (in 12-point Helvetica).
fn two_pages(first: &str, second: &str) -> Vec<u8> {
    let page = |extra: &str| format!("BT /F1 12 Tf 20 250 Td (Terms of supply) Tj ET\n{extra}");
    let (first, second) = (page(first), page(second));
    pages_drawing(2, "", false, |_| String::new(), &[], move |i| if i == 0 { first.clone() } else { second.clone() })
}

const CLAUSE_X: &str = "BT /F1 12 Tf 20 150 Td (Seller is liable) Tj ET";
const CLAUSE_X_BOX: &str = "0 g 18 145 95 17 re f";

#[test]
fn a_page_replaced_by_a_copy_of_another_is_refused() {
    // Base: page 1 "Terms", page 2 "Terms" and a clause. Edited (a rewrite): both pages are
    // page 2 with its clause redacted. Either could be page 1 with a box over nothing, or page 2
    // redacted: on fd434a2 page 1 was taken for the first, page 2 for the second, and the copy
    // passed as a redaction.
    let base = two_pages("", CLAUSE_X);
    let copied = two_pages(CLAUSE_X_BOX, CLAUSE_X_BOX);
    assert!(refused(run(&base, &copied)).contains("cannot be told apart"));
    // One page redacted in place is certain.
    let redacted = two_pages("", CLAUSE_X_BOX);
    let inspection = run(&base, &redacted).unwrap();
    assert_eq!((inspection.content.pages_changed_outside_redactions, inspection.redaction_areas.len()), (0, 1), "{inspection:?}");
}

#[test]
fn near_identical_pages_swapped_are_reported_moved() {
    // Page 2 is page 1 and a clause. A rewrite redacts the clause and puts page 2 first: the
    // pages changed order, which fd434a2 did not report (only pages unchanged elsewhere were
    // moves).
    let base = two_pages("", CLAUSE_X);
    let swapped = {
        let page = |extra: &str| format!("BT /F1 12 Tf 20 250 Td (Terms of supply) Tj ET\n{extra}");
        let (first, second) = (page(CLAUSE_X_BOX), page(""));
        pages_drawing(2, "", false, |_| String::new(), &[], move |i| if i == 0 { first.clone() } else { second.clone() })
    };
    let inspection = run(&base, &swapped).unwrap();
    assert_eq!(inspection.pages, PageChanges { before: 2, after: 2, moved: 1, ..PageChanges::default() }, "{inspection:?}");
    assert_eq!(inspection.redaction_areas.first().map(|area| area.0), Some(1), "{inspection:?}");
}

/// Page `own`'s glyphs, as far as pairing goes: `glyphs` glyphs (glyph `g` is `shared(g)`, at
/// the same place on every page), and the page's own number.
fn synthetic_glyphs(own: usize, shared: &dyn Fn(usize) -> char, glyphs: usize) -> structure::Glyphs {
    let mut out = structure::Glyphs::default();
    for g in 0..glyphs {
        let (x, y) = ((g % 30) as f32 * 6.0, 10.0 + (g / 30) as f32 * 12.0);
        out.page.chars.push(shared(g));
        out.page.glyphs.push((out.page.chars.len() as u32, [x, y, x + 6.0, y + 12.0]));
    }
    for (k, c) in format!("{own:04}").chars().enumerate() {
        out.page.chars.push(c);
        out.page.glyphs.push((out.page.chars.len() as u32, [20.0 + k as f32 * 6.0, 580.0, 26.0 + k as f32 * 6.0, 592.0]));
    }
    out
}

/// Pair 2,000 changed base pages with 2,000 changed edited pages, page `i` on both sides drawing
/// glyph `g` as `shared(i, g)`: the pairs, the page changes and how long it took.
fn pair_synthetic(shared: &dyn Fn(usize, usize) -> char, glyphs: usize, limits: Limits) -> (Result<Pairs, Fail>, PageChanges, std::time::Duration) {
    let facts = |tag: &[u8]| -> Vec<PageFacts> {
        (0..MAX_PAGES)
            .map(|i| PageFacts::for_tests(synthetic_glyphs(i, &|g| shared(i, g), glyphs), structure::sha256(&[tag, &i.to_le_bytes()])))
            .collect()
    };
    let (base, edited) = (facts(b"base"), facts(b"edited"));
    let mut budget = Budget::new(limits);
    let mut changes = PageChanges::default();
    let started = std::time::Instant::now();
    // Each page's render, as far as pairing goes: page `i`'s own (its number differs).
    let mut thumbs = |_: bool, index: usize, _: &mut Budget| -> Result<visual::Thumb, Fail> { Ok(visual::Thumb::for_tests(index)) };
    let result = match_pages(&base, &edited, &mut changes, &Votes::default(), &mut thumbs, &mut budget);
    (result, changes, started.elapsed())
}

#[test]
fn pairing_two_thousand_near_identical_pages_is_bounded() {
    use std::time::Duration;
    // Every page the same 40 glyphs but for its number: each pairs with its own (by its render:
    // every glyph is shared by more pages than tells them apart).
    let (result, changes, took) = pair_synthetic(&|_, g| char::from(b'a' + (g % 26) as u8), 40, Limits::default());
    let pairs = result.map_err(|error| format!("{error:?}")).unwrap();
    assert!(pairs.iter().all(|(old, new)| old == new) && pairs.len() == MAX_PAGES, "{changes:?}");
    assert_eq!(changes.moved, 0);
    assert!(took < Duration::from_secs(10), "{took:?}");
    // Pages alike in groups of sixteen, with many glyphs each (every feature shared by sixteen
    // pages, the most that still counts): the pairing's work is bounded, and past it the
    // inspection is too complex.
    let (result, _, took) =
        pair_synthetic(&|i, g| char::from_u32(0x4E00 + (i / 16) as u32 * 1_200 + g as u32).unwrap_or('x'), 1_200, Limits::default());
    assert!(matches!(result, Err(Fail::TooComplex)), "not too complex");
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// Glyph `g` of page `i` in pages built so that each shares one glyph with about 600 others
/// (each glyph with 15 others, a different 15 for each glyph): about a million candidate pairs.
fn spread(i: usize, g: usize) -> char {
    let multiplier = [1usize, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79][g % 20] + 2 * (g / 20) * 83;
    let group = (i * multiplier % MAX_PAGES) / 16;
    char::from_u32(0x4E00 + (g * 200 + group) as u32).unwrap_or('x')
}

#[test]
fn the_pairing_s_working_memory_is_charged_to_its_bound() {
    // The minor item of the follow-up 3 review: about a million candidate pairs were weighed in
    // some 220 MB that no bound charged. The same pairing, charged: within the default bound
    // (64 MiB) it is too complex; with room for it, it completes, each page with its own.
    use std::time::Duration;
    let (result, _, took) = pair_synthetic(&spread, 40, Limits::default());
    assert!(matches!(result, Err(Fail::TooComplex)), "not too complex: {:?}", result.map(|pairs| pairs.len()));
    assert!(took < Duration::from_secs(10), "{took:?}");
    let roomy = Limits { pairing_bytes: usize::MAX, pairing: u64::MAX, ..Limits::default() };
    let (result, _, _) = pair_synthetic(&spread, 40, roomy);
    let pairs = result.map_err(|error| format!("{error:?}")).unwrap();
    assert!(pairs.iter().all(|(old, new)| old == new) && pairs.len() == MAX_PAGES);
    // A budget that cannot hold one base page's features stops at once.
    let tiny = Limits { pairing_bytes: 1_000, ..Limits::default() };
    assert!(matches!(pair_synthetic(&spread, 40, tiny).0, Err(Fail::TooComplex)));
}

// ── R6b-32: named destinations and links follow their pages ─────────────────────────────────

/// Four pages; names "p1" … "p4" going to them, in the `/Names /Dests` tree (object 40, a
/// leaf under the root 41) or the catalog's `/Dests` dictionary (object 42).
fn named(tree: bool) -> Vec<u8> {
    let entries: Vec<String> = (0..4).map(|i| format!("(p{}) [{} 0 R /XYZ 0 300 0]", i + 1, 10 + i)).collect();
    let dests: Vec<String> = (0..4).map(|i| format!("/p{} [{} 0 R /XYZ 0 300 0]", i + 1, 10 + i)).collect();
    if tree {
        pages(
            4,
            "/Names << /Dests 41 0 R >>",
            false,
            |_| String::new(),
            &[(40, format!("<< /Names [{}] /Limits [(p1) (p4)] >>", entries.join(" "))), (41, "<< /Kids [40 0 R] >>".to_string())],
        )
    } else {
        pages(4, "/Dests 42 0 R", false, |_| String::new(), &[(42, format!("<< {} >>", dests.join(" ")))])
    }
}

#[test]
fn named_destinations_follow_their_pages() {
    for tree in [true, false] {
        let form = if tree { "/Names /Dests" } else { "/Dests" };
        let base = named(tree);
        assert_eq!(run(&base, &base).unwrap().destinations, DestinationChanges::default(), "{form}");
        // The reviewer's cases (I4): rotating or annotating the page a name targets, deleting or
        // moving a page. Each was refused ("the document's /Names changed").
        // (case, edits, named destinations that followed their page, pages deleted)
        let cases: Vec<(&str, Vec<Edit>, u32, u32)> = vec![
            ("rotate a target", vec![Edit::RotatePages { pages: vec![1], degrees: 90 }], 0, 0),
            ("annotate a target", vec![sticky_note(1)], 0, 0),
            ("delete the last page", vec![Edit::DeletePages { pages: vec![3] }], 1, 1),
            ("delete the first page", vec![Edit::DeletePages { pages: vec![0] }], 4, 1),
            ("move the last page first", vec![Edit::MovePages { pages: vec![3], to: 0 }], 4, 0),
        ];
        for (case, edits, followed, deleted) in cases {
            let inspection = run(&base, &session(&base, edits)).unwrap_or_else(|error| panic!("{form}, {case}: {error:?}"));
            assert_eq!(inspection.destinations, DestinationChanges { named: followed, links: 0 }, "{form}, {case}: {inspection:?}");
            assert_eq!(inspection.pages.deleted, deleted, "{form}, {case}");
            assert_eq!(inspection.content, ContentChanges::default(), "{form}, {case}: {inspection:?}");
        }
    }
    // Any other change of a named destination fails: another page, another view, a name added,
    // a name removed whose page is still there.
    let base = named(true);
    let leaf = |entries: &str, limits: &str| update_objects(&base, &[(40, &format!("<< /Names [{entries}] /Limits [{limits}] >>"))], 43);
    let all = "(p1) [10 0 R /XYZ 0 300 0] (p2) [11 0 R /XYZ 0 300 0] (p3) [12 0 R /XYZ 0 300 0]";
    for (case, edited) in [
        ("another page", leaf(&format!("{all} (p4) [10 0 R /XYZ 0 300 0]"), "(p1) (p4)")),
        ("another view", leaf(&format!("{all} (p4) [13 0 R /Fit]"), "(p1) (p4)")),
        ("a name added", leaf(&format!("{all} (p4) [13 0 R /XYZ 0 300 0] (p5) [13 0 R /Fit]"), "(p1) (p5)")),
        ("a name removed", leaf(all, "(p1) (p3)")),
    ] {
        assert!(refused(run(&base, &edited)).contains("named destination"), "{case}");
    }
    // Other name trees (`/JavaScript`, …) are still compared whole.
    let scripted = update_objects(&base, &[(1, "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 41 0 R /JavaScript << /Names [] >> >> >>")], 43);
    assert!(refused(run(&base, &scripted)).contains("/Names"));
}

#[test]
fn a_redaction_and_a_page_delete_on_a_file_with_named_destinations_are_accepted() {
    // R6b-37, the reviewer's I-A (`i4_named_destinations`): the Session's rewrite writes the name
    // of a deleted page as `(p1) [null /XYZ 0 300 0]`, a destination whose page is null. e91797d
    // read that as another destination and refused the save; it goes to the page that was
    // deleted, and is reported as having followed it.
    for tree in [true, false] {
        let form = if tree { "/Names /Dests" } else { "/Dests" };
        let base = named(tree);
        // (case, pages redacted, page deleted, page delete first)
        for (case, redacted, deleted, delete_first) in [
            ("redact 3, delete 1", vec![2], 0, false),
            ("delete 1, then redact 3", vec![2], 0, true),
            ("redact 3 and 4, delete 1", vec![2, 3], 0, false),
        ] {
            let mut edits: Vec<Edit> = Vec::new();
            if delete_first {
                edits.push(Edit::DeletePages { pages: vec![deleted] });
            }
            for &page in &redacted {
                // After the delete, the pages are one earlier.
                let page = if delete_first && page > deleted { page - 1 } else { page };
                edits.extend(redact_number(page));
            }
            if !delete_first {
                edits.push(Edit::DeletePages { pages: vec![deleted] });
            }
            let inspection = run(&base, &session(&base, edits)).unwrap_or_else(|error| panic!("{form}, {case}: {error:?}"));
            assert_eq!(inspection.pages, PageChanges { before: 4, after: 3, deleted: 1, ..PageChanges::default() }, "{form}, {case}: {inspection:?}");
            assert_eq!(inspection.destinations, DestinationChanges { named: 4, links: 0 }, "{form}, {case}: {inspection:?}");
            assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{form}, {case}: {inspection:?}");
            assert_eq!(inspection.redaction_areas.len(), redacted.len(), "{form}, {case}: {inspection:?}");
        }
    }
    // A null page where the base page is still there is not a deleted page's destination.
    let base = named(true);
    let all = "(p1) [null /XYZ 0 300 0] (p2) [11 0 R /XYZ 0 300 0] (p3) [12 0 R /XYZ 0 300 0] (p4) [13 0 R /XYZ 0 300 0]";
    let edited = update_objects(&base, &[(40, &format!("<< /Names [{all}] /Limits [(p1) (p4)] >>"))], 43);
    assert!(refused(run(&base, &edited)).contains("named destination"));
}

/// Four pages; page 1 links to page 3 (object 43) by `/Dest`, or by a go-to action.
fn linked(action: bool) -> Vec<u8> {
    let target = if action { "/A << /S /GoTo /D [12 0 R /Fit] >>" } else { "/Dest [12 0 R /Fit]" };
    pages(
        4,
        "",
        false,
        |i| if i == 0 { "/Annots [43 0 R]".to_string() } else { String::new() },
        &[(43, format!("<< /Type /Annot /Subtype /Link /Rect [20 20 120 40] /Border [0 0 0] {target} >>"))],
    )
}

#[test]
fn links_follow_their_pages() {
    for action in [false, true] {
        let form = if action { "a go-to action" } else { "/Dest" };
        let base = linked(action);
        // The reviewer's cases (M2): each reported the link as changed, though nothing changed it.
        let cases: Vec<(&str, Vec<Edit>, u32)> = vec![
            ("rotate its target", vec![Edit::RotatePages { pages: vec![2], degrees: 90 }], 0),
            ("annotate its target", vec![sticky_note(2)], 0),
            ("delete an unrelated page", vec![Edit::DeletePages { pages: vec![3] }], 0),
            ("delete a page before its target", vec![Edit::DeletePages { pages: vec![1] }], 1),
            ("move its target first", vec![Edit::MovePages { pages: vec![2], to: 0 }], 1),
            ("delete its target", vec![Edit::DeletePages { pages: vec![2] }], 1),
        ];
        for (case, edits, followed) in cases {
            let inspection = run(&base, &session(&base, edits)).unwrap_or_else(|error| panic!("{form}, {case}: {error:?}"));
            assert_eq!(inspection.destinations, DestinationChanges { named: 0, links: followed }, "{form}, {case}: {inspection:?}");
            assert_eq!(inspection.annotations.changed, 0, "{form}, {case}: {inspection:?}");
            assert_eq!(inspection.annotations.removed, 0, "{form}, {case}: {inspection:?}");
        }
    }
    // Retargeted: another page, another view.
    let base = linked(false);
    for (case, dest) in [("another page", "[13 0 R /Fit]"), ("another view", "[12 0 R /XYZ 0 0 2]")] {
        let edited =
            update_objects(&base, &[(43, &format!("<< /Type /Annot /Subtype /Link /Rect [20 20 120 40] /Border [0 0 0] /Dest {dest} >>"))], 44);
        assert!(refused(run(&base, &edited)).contains("link's destination"), "{case}");
    }
    // In a rewrite (an applied redaction), a link written again with its destination is the
    // same link; one going elsewhere is refused.
    let redacted = session(&base, redact_number(1));
    let inspection = run(&base, &redacted).unwrap();
    assert_eq!((inspection.annotations, inspection.destinations), (AnnotationChanges::default(), DestinationChanges::default()), "{inspection:?}");
}

#[test]
fn a_base_page_drawn_with_what_viewers_render_differently_is_checked_too() {
    // M1: the base page's form, drawn once, is a knockout group; the rewrite draws the same path
    // plainly and redacts a word. hayro draws both alike; other viewers need not.
    let file = |content: &str, form: bool| {
        let mut objects: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        objects.insert(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        objects.insert(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec());
        let xobjects = if form { "/XObject << /Fm 5 0 R >>" } else { "" };
        objects.insert(
            3,
            format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 {HELVETICA} >> {xobjects} >> >>").into_bytes(),
        );
        objects.insert(4, stream("", content.as_bytes()));
        if form {
            objects.insert(
                5,
                stream("/Type /XObject /Subtype /Form /BBox [0 0 200 300] /Group << /S /Transparency /K true >>", b"0 g 20 200 50 50 re f"),
            );
        }
        assemble(&objects)
    };
    let words = "BT /F1 12 Tf 20 150 Td (Alpha) Tj ET";
    let base = file(&format!("/Fm Do\n{words}\nBT /F1 12 Tf 150 20 Td (X) Tj ET"), true);
    let edited = file(&format!("0 g 20 200 50 50 re f\n{words}\n0 g 146 14 16 18 re f"), false);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!(inspection.content.pages_changed_outside_redactions, 1, "{inspection:?}");
}

// ── the structure audit: what viewers could resolve differently ─────────────────────────────

#[test]
fn a_name_tree_viewers_could_resolve_differently_is_refused() {
    let base = named(true);
    let leaf = |entries: &str, limits: &str| update_objects(&base, &[(40, &format!("<< /Names [{entries}] {limits} >>"))], 43);
    let cases = [
        (
            "a key named twice",
            leaf("(p1) [10 0 R /Fit] (p1) [11 0 R /Fit] (p2) [11 0 R /XYZ 0 300 0] (p3) [12 0 R /XYZ 0 300 0] (p4) [13 0 R /XYZ 0 300 0]", ""),
            "out of order or named twice",
        ),
        (
            "keys out of order",
            leaf(
                "(p2) [11 0 R /XYZ 0 300 0] (p1) [10 0 R /XYZ 0 300 0] (p3) [12 0 R /XYZ 0 300 0] (p4) [13 0 R /XYZ 0 300 0]",
                "/Limits [(p2) (p4)]",
            ),
            "out of order or named twice",
        ),
        (
            "limits that do not bound the node",
            leaf(
                "(p1) [10 0 R /XYZ 0 300 0] (p2) [11 0 R /XYZ 0 300 0] (p3) [12 0 R /XYZ 0 300 0] (p4) [13 0 R /XYZ 0 300 0]",
                "/Limits [(p1) (p3)]",
            ),
            "/Limits",
        ),
        ("a key that is not a string", leaf("/p1 [10 0 R /XYZ 0 300 0]", ""), "not a string"),
        ("a key without a value", leaf("(p1)", ""), "without a value"),
    ];
    for (case, edited, says) in cases {
        let message = refused(run(&base, &edited));
        assert!(message.contains(says), "{case}: {message}");
    }
    // Across leaves: the second leaf's keys must come after the first's.
    let split = |second: &str| {
        let leaf = |num: u32, entries: &str, limits: &str| (num, format!("<< /Names [{entries}] /Limits [{limits}] >>"));
        pages(
            4,
            "/Names << /Dests 41 0 R >>",
            false,
            |_| String::new(),
            &[
                leaf(40, "(p1) [10 0 R /XYZ 0 300 0] (p2) [11 0 R /XYZ 0 300 0]", "(p1) (p2)"),
                (44, second.to_string()),
                (41, "<< /Kids [40 0 R 44 0 R] >>".to_string()),
            ],
        )
    };
    let sound = split("<< /Names [(p3) [12 0 R /XYZ 0 300 0] (p4) [13 0 R /XYZ 0 300 0]] /Limits [(p3) (p4)] >>");
    assert!(run(&sound, &sound).is_ok());
    let overlapping = split("<< /Names [(p2) [12 0 R /XYZ 0 300 0] (p4) [13 0 R /XYZ 0 300 0]] /Limits [(p2) (p4)] >>");
    assert!(matches!(run(&overlapping, &overlapping), Err(InspectError::Base(message)) if message.contains("out of order or named twice")));
    // A node with both /Kids and /Names, and a tree that loops.
    let both = update_objects(&base, &[(41, "<< /Kids [40 0 R] /Names [] >>")], 43);
    assert!(refused(run(&base, &both)).contains("both /Kids and /Names"));
    let looping = update_objects(&base, &[(40, "<< /Kids [41 0 R] >>")], 43);
    assert!(refused(run(&base, &looping)).contains("loops"));
}

#[test]
fn an_outline_whose_next_chain_loops_is_refused() {
    let base = pages(3, "", true, |_| String::new(), &[]);
    // The last entry's /Next goes back to the first.
    let looping = update_objects(&base, &[(32, "<< /Title (Page 3) /Parent 3 0 R /Prev 31 0 R /Next 30 0 R /Dest [12 0 R /Fit] >>")], 33);
    assert!(refused(run(&base, &looping)).contains("the outline loops"));
    // Back to itself.
    let itself = update_objects(&base, &[(31, "<< /Title (Page 2) /Parent 3 0 R /Prev 30 0 R /Next 31 0 R /Dest [11 0 R /Fit] >>")], 33);
    assert!(refused(run(&base, &itself)).contains("the outline loops"));
}

#[test]
fn page_attributes_the_readers_inherit_differently_are_refused() {
    // hayro skips a value it cannot read and inherits the parent's (or a default); the inspector
    // reads the page's own. Each would let the two readers see another page box or rotation.
    let base = pages(2, "", false, |_| String::new(), &[]);
    let page = |extra: &str| format!("<< /Type /Page /Parent 2 0 R /Contents 20 0 R /Resources << /Font << /F1 {HELVETICA} >> >> {extra} >>");
    let tree = |extra: &str| format!("<< /Type /Pages /Kids [10 0 R 11 0 R] /Count 2 /MediaBox [0 0 200 300] {extra} >>");
    let cases: [(&str, Vec<(u32, String)>); 5] = [
        ("a media box of three numbers", vec![(10, page("/MediaBox [0 0 200]"))]),
        ("a media box of five numbers", vec![(10, page("/MediaBox [0 0 100 100 5]"))]),
        ("a crop box with a reference in it", vec![(10, page("/CropBox [0 0 100 12 0 R]")), (12, "100".into())]),
        ("a crop box that is a name", vec![(10, page("/CropBox /Full")), (2, tree("/CropBox [0 0 100 100]"))]),
        ("a rotation hayro cannot read as a 32-bit number", vec![(10, page("/Rotate 4294967400")), (2, tree("/Rotate 90"))]),
    ];
    for (case, objects) in cases {
        let objects: Vec<(u32, &str)> = objects.iter().map(|(num, body)| (*num, body.as_str())).collect();
        let edited = update_objects(&base, &objects, 22);
        let message = match run(&base, &edited) {
            Err(InspectError::Edited(message)) => message,
            other => panic!("{case}: {other:?}"),
        };
        assert!(message.contains("page") || message.contains("readers disagree"), "{case}: {message}");
    }
    // Inherited alike: a rotation and boxes set on the page tree are each page's.
    let inherited = update_objects(&base, &[(2, &tree("/Rotate 90 /CropBox [10 10 190 290]"))], 22);
    let inspection = run(&base, &inherited).unwrap();
    assert_eq!(inspection.pages.rotated, 2, "{inspection:?}");
}

#[test]
fn every_object_read_parses_alike_in_both_readers() {
    // Values each parser writes its own way (numbers, escapes, stray tokens between entries)
    // parse alike: accepted, and compared.
    let base = pages(2, "", false, |_| String::new(), &[]);
    let body = format!(
        "<< /Type /Page /Parent 2 0 R /Contents 20 0 R (stray) /Rotate 90 /Resources << /Font << /F1 {HELVETICA} >> >> \
         /PieceInfo << /A#20B [1. -.5 +3 (a\\(b\\)\\101) <41 42>] >> >>"
    );
    let edited = update_objects(&base, &[(10, &body)], 22);
    let inspection = run(&base, &edited).unwrap();
    assert_eq!((inspection.pages.rotated, inspection.content.pages_with_content_changed), (1, 1), "{inspection:?}");
    // What the two readers parse is compared object by object: a value one reader parses
    // otherwise is refused (the shapes as hayro gave them, one changed).
    let doc = pdfcraft_cos::Document::open(Arc::new(edited.clone())).unwrap();
    let mut budget = Budget::new(Limits::default());
    let sections = structure::sections(&doc).unwrap();
    let (_, known) = structure::read(&doc, &mut budget, Arc::new(std::collections::HashMap::new()), sections, structure::Tolerated::Base).unwrap();
    let hayro = pdfcraft_render::content::ContentDocument::open(Arc::new(edited), u64::MAX).unwrap();
    let read = known.objects_read(&doc);
    let mut shapes = hayro.object_shapes(&read);
    assert!(known.agree_shapes(&doc, &mut budget, &read, &shapes).is_ok());
    let page = read.iter().position(|object| object.0 == 10).unwrap();
    if let Some(Some(shape)) = shapes.get_mut(page) {
        let last = shape.len() - 1;
        shape[last] ^= 1;
    }
    let refused = known.agree_shapes(&doc, &mut budget, &read, &shapes);
    assert!(matches!(refused, Err(Fail::Read(message)) if message.contains("hayro parses object 10 0 R otherwise")));
}
