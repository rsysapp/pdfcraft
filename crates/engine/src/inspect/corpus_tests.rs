//! Embed build: ordinary files are not refused. Every PDF sample in the repository inspects clean
//! against itself and after the edits a host accepts (unchanged, a rotation, a note, a page
//! deleted, a page moved, a page deleted with a note, a whole line of text redacted, and a line
//! redacted with a page deleted, in both orders); a directory of other samples is checked the
//! same way on request:
//!
//! ```text
//! PDFCRAFT_INSPECT_SAMPLES=/path/to/pdfs cargo test -p pdfcraft-engine --lib \
//!     inspect::corpus_tests::samples_in_a_directory -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use super::*;
use crate::{Edit, NewAnnotation, Session, Shape, Style, rect_quad};

/// The repository's PDF samples (`git ls-files '*.pdf'`).
const REPOSITORY_SAMPLES: [(&str, &[u8]); 2] = [
    ("crates/sign/tests/data/openssl-signed.pdf", include_bytes!("../../../sign/tests/data/openssl-signed.pdf")),
    ("crates/ui-egui/tests/data/form.pdf", include_bytes!("../../../ui-egui/tests/data/form.pdf")),
];

fn run(base: &[u8], edited: &[u8]) -> Result<Inspection, InspectError> {
    inspect(base, edited, InspectLimits::default())
}

/// A session as the embed build sets it up: it keeps the document information (R6b-45), with a
/// clock that would otherwise stamp the modification date.
fn embed_session() -> Session {
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    session.set_keep_document_information(true);
    session
}

fn edit(base: &[u8], edits: Vec<Edit>) -> Vec<u8> {
    let mut session = embed_session();
    let id = session.open("sample.pdf", None, Arc::new(base.to_vec()), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

/// A sticky note added to page `page`.
fn note(page: usize) -> Edit {
    let shape = Shape::Note { at: [20.0, 20.0], icon: Default::default() };
    Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: "Checked".into(), author: "Reviewer".into() })
}

/// A whole line of page `page`'s text (top first, the first with at least five glyphs whose box
/// touches no other glyph), as a redaction mark applied: a box around every glyph whose em box
/// reaches into the line's band, half a point higher and lower and three points wider. A whole line, since the redactor sets again
/// what it keeps of a line it cuts (R6b-8).
fn redact_line(bytes: &[u8], page: usize) -> Option<Vec<Edit>> {
    let hayro = pdfcraft_render::content::ContentDocument::open(Arc::new(bytes.to_vec()), u64::MAX).ok()?;
    let glyphs = hayro.content(page).ok()?.glyphs;
    let rects: Vec<[f32; 4]> = (0..glyphs.len()).filter_map(|index| glyphs.glyph(index).map(|(_, rect)| rect)).collect();
    // Bands by the em boxes' vertical centre, to a point, top first.
    let mut centres: Vec<i64> = rects.iter().map(|rect| ((rect[1] + rect[3]) / 2.0).round() as i64).collect();
    centres.sort_unstable_by(|a, b| b.cmp(a));
    centres.dedup();
    let overlaps = |rect: &[f32; 4], band: (f32, f32)| rect[1] < band.1 && rect[3] > band.0;
    for centre in centres {
        let members: Vec<&[f32; 4]> = rects.iter().filter(|rect| ((rect[1] + rect[3]) / 2.0).round() as i64 == centre).collect();
        let band = members.iter().fold((f32::MAX, f32::MIN), |band, rect| (band.0.min(rect[1]), band.1.max(rect[3])));
        // Every glyph reaching into the band, and the band they make together.
        let line: Vec<&[f32; 4]> = rects.iter().filter(|rect| overlaps(rect, band)).collect();
        let union =
            line.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |u, r| [u[0].min(r[0]), u[1].min(r[1]), u[2].max(r[2]), u[3].max(r[3])]);
        let padded = (union[1] - 0.5, union[3] + 0.5);
        if line.len() < 5 || rects.iter().filter(|rect| overlaps(rect, padded)).count() != line.len() {
            continue;
        }
        // Wider than the em boxes on the left and right, where no other line is: the redactor
        // judges a glyph by its font's box, which can reach past its em box.
        let rect = [f64::from(union[0]) - 3.0, f64::from(padded.0), f64::from(union[2]) + 3.0, f64::from(padded.1)];
        let shape = Shape::Redact { quads: vec![rect_quad(rect)], overlay: String::new(), look: Default::default() };
        return Some(vec![
            Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: String::new(), author: "Reviewer".into() }),
            Edit::ApplyRedactions { pages: None },
        ]);
    }
    None
}

/// `edit`, or `None` where the Session refuses an edit (a signed file is not rewritten).
fn try_edit(base: &[u8], edits: Vec<Edit>) -> Option<Vec<u8>> {
    let mut session = embed_session();
    let id = session.open("sample.pdf", None, Arc::new(base.to_vec()), None).ok()?;
    for edit in edits {
        session.apply(id, edit).ok()?;
    }
    session.save_bytes(id).ok().map(|bytes| bytes.as_ref().clone())
}

/// The document information a save through the embed build's session changes: none, though its
/// clock would stamp the modification date (R6b-45).
const STAMPED: [&str; 0] = [];

/// A rewrite that is nothing but redactions on `redacted` pages and `deleted` pages deleted.
fn assert_redacted(name: &str, inspection: &Inspection, count: u32, deleted: u32) {
    assert!(!inspection.base_kept, "{name}: {inspection:?}");
    assert_eq!(inspection.info.changed, STAMPED, "{name}: {inspection:?}");
    assert_eq!(inspection.structure_removed, StructureRemovals::default(), "{name}: {inspection:?}");
    assert_eq!(inspection.pages, PageChanges { before: count, after: count - deleted, deleted, ..PageChanges::default() }, "{name}: {inspection:?}");
    assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 }, "{name}: {inspection:?}");
    assert_eq!(inspection.content.pages_changed_outside_redactions, 0, "{name}: {inspection:?}");
    assert!(!inspection.filled_areas.is_empty(), "{name}: {inspection:?}");
}

/// Nothing changed but what `pages` says, the outline facts, `removed` annotations (those of a
/// deleted page) and the document information's keys `info`.
fn assert_only(name: &str, inspection: &Inspection, pages: PageChanges, removed: u32, info: &[&str]) {
    assert_eq!(inspection.pages, pages, "{name}: {inspection:?}");
    assert_eq!(inspection.content, ContentChanges::default(), "{name}: {inspection:?}");
    assert_eq!(inspection.text, TextChanges::default(), "{name}: {inspection:?}");
    assert_eq!(inspection.annotations, AnnotationChanges { removed, ..AnnotationChanges::default() }, "{name}: {inspection:?}");
    assert_eq!(inspection.form_fields_changed, 0, "{name}: {inspection:?}");
    assert_eq!(inspection.structure_removed, StructureRemovals::default(), "{name}: {inspection:?}");
    assert_eq!(inspection.info.changed, info, "{name}: {inspection:?}");
}

/// Check one sample: unchanged, rotated, annotated, and (with two pages or more) a page deleted,
/// a page moved, and a page deleted with a note added. Returns a line describing what was found.
fn check(name: &str, bytes: &[u8]) -> String {
    let inspection = run(bytes, bytes).unwrap_or_else(|error| panic!("{name}, unchanged: {error:?}"));
    let count = inspection.pages.before;
    assert!(inspection.base_kept, "{name}");
    assert_only(name, &inspection, PageChanges { before: count, after: count, ..PageChanges::default() }, 0, &[]);
    assert_eq!(inspection.outlines, OutlineChanges::default(), "{name}");
    let rotated = run(bytes, &edit(bytes, vec![Edit::RotatePages { pages: vec![0], degrees: 90 }]))
        .unwrap_or_else(|error| panic!("{name}, rotated: {error:?}"));
    assert_only(name, &rotated, PageChanges { before: count, after: count, rotated: 1, ..PageChanges::default() }, 0, &STAMPED);
    assert_eq!(rotated.outlines, OutlineChanges::default(), "{name}, rotated");
    let annotated = run(bytes, &edit(bytes, vec![note(0)])).unwrap_or_else(|error| panic!("{name}, annotated: {error:?}"));
    assert_eq!(annotated.annotations, AnnotationChanges { comments: 1, ..AnnotationChanges::default() }, "{name}: {annotated:?}");
    assert_eq!(
        (annotated.info.changed.as_slice(), annotated.structure_removed),
        (STAMPED.map(String::from).as_slice(), StructureRemovals::default()),
        "{name}"
    );
    assert_eq!((annotated.content, annotated.text, annotated.pages.rotated), (ContentChanges::default(), TextChanges::default(), 0), "{name}");
    assert_eq!((annotated.outlines, annotated.destinations), (OutlineChanges::default(), DestinationChanges::default()), "{name}");
    let mut line = format!("{name}: {count} page(s), clean; rotated: clean; annotated: 1 comment");
    if count >= 2 {
        let last = count as usize - 1;
        let deleted = run(bytes, &edit(bytes, vec![Edit::DeletePages { pages: vec![0] }]))
            .unwrap_or_else(|error| panic!("{name}, first page deleted: {error:?}"));
        // The deleted page's annotations went with it.
        let annotations = deleted.annotations.removed;
        assert_only(name, &deleted, PageChanges { before: count, after: count - 1, deleted: 1, ..PageChanges::default() }, annotations, &STAMPED);
        assert_eq!(deleted.outlines.titles_changed + deleted.outlines.added + deleted.outlines.removed, 0, "{name}: {deleted:?}");
        let moved = run(bytes, &edit(bytes, vec![Edit::MovePages { pages: vec![last], to: 0 }]))
            .unwrap_or_else(|error| panic!("{name}, last page moved first: {error:?}"));
        assert_eq!(moved.pages.deleted + moved.pages.inserted, 0, "{name}: {moved:?}");
        assert!(moved.pages.moved >= 1, "{name}: {moved:?}");
        assert_eq!(moved.content, ContentChanges::default(), "{name}: {moved:?}");
        assert_eq!(moved.text, TextChanges::default(), "{name}: {moved:?}");
        assert_eq!(
            (moved.info.changed.as_slice(), moved.structure_removed),
            (STAMPED.map(String::from).as_slice(), StructureRemovals::default()),
            "{name}"
        );
        assert_eq!(moved.outlines.titles_changed + moved.outlines.added + moved.outlines.removed, 0, "{name}: {moved:?}");
        let both = run(bytes, &edit(bytes, vec![Edit::DeletePages { pages: vec![0] }, note(0)]))
            .unwrap_or_else(|error| panic!("{name}, first page deleted and a note added: {error:?}"));
        assert_eq!(both.pages, PageChanges { before: count, after: count - 1, deleted: 1, ..PageChanges::default() }, "{name}: {both:?}");
        assert_eq!(both.annotations, AnnotationChanges { comments: 1, removed: annotations, ..AnnotationChanges::default() }, "{name}: {both:?}");
        assert_eq!((both.content, both.text), (ContentChanges::default(), TextChanges::default()), "{name}: {both:?}");
        assert_eq!(
            (both.info.changed.as_slice(), both.structure_removed),
            (STAMPED.map(String::from).as_slice(), StructureRemovals::default()),
            "{name}"
        );
        line.push_str(&format!(
            "; first page deleted: accepted ({annotations} annotation(s) gone with it), {:?}, {:?}; last page moved first: accepted ({} moved), {:?}, {:?}; first page deleted and a note added: accepted, {:?}",
            deleted.outlines, deleted.destinations, moved.pages.moved, moved.outlines, moved.destinations, both.outlines
        ));
    }
    // A whole line of text redacted on the first page; with two pages or more, together with a
    // page delete, in both orders: a line of the first page then the last page deleted, and the
    // first page deleted then a line of the last page.
    if let Some(redaction) = redact_line(bytes, 0)
        && let Some(saved) = try_edit(bytes, redaction.clone())
    {
        let redacted = run(bytes, &saved).unwrap_or_else(|error| panic!("{name}, a line redacted: {error:?}"));
        assert_redacted(&format!("{name}, a line redacted"), &redacted, count, 0);
        line.push_str("; a line redacted: accepted");
        if count >= 2 {
            let last = count as usize - 1;
            let mut edits = redaction;
            edits.push(Edit::DeletePages { pages: vec![last] });
            let first =
                run(bytes, &edit(bytes, edits)).unwrap_or_else(|error| panic!("{name}, a line redacted, then the last page deleted: {error:?}"));
            assert_redacted(&format!("{name}, a line redacted, then the last page deleted"), &first, count, 1);
            if let Some(redaction) = redact_line(bytes, last) {
                let mut edits = vec![Edit::DeletePages { pages: vec![0] }];
                edits.extend(redaction.into_iter().map(|edit| match edit {
                    Edit::AddAnnotation(mut annotation) => {
                        annotation.page = last - 1;
                        Edit::AddAnnotation(annotation)
                    }
                    other => other,
                }));
                let second = run(bytes, &edit(bytes, edits))
                    .unwrap_or_else(|error| panic!("{name}, the first page deleted, then a line of the last redacted: {error:?}"));
                assert_redacted(&format!("{name}, the first page deleted, then a line of the last redacted"), &second, count, 1);
                line.push_str(&format!("; with a page delete, in both orders: accepted, {:?}, {:?}", first.outlines, second.outlines));
            }
        }
    }
    line
}

/// The embed build's session for a save: marks redactions only (R6b-49).
fn marking(base: &[u8], edits: Vec<Edit>) -> Vec<u8> {
    let mut session = embed_session();
    session.set_redaction_marks_only(true);
    let id = session.open("sample.pdf", None, Arc::new(base.to_vec()), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

fn shape(page: usize, shape: Shape) -> Edit {
    Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: String::new(), author: "Reviewer".into() })
}

/// Follow-up 8 (re-review M3): every tool the embed build's permissions open, as a save sends
/// it (an appended update), each accepted with the facts it should give; and a line marked and
/// applied on the server (R6b-49), accepted with only the redaction.
fn check_tools(name: &str, bytes: &[u8]) -> String {
    let count = run(bytes, bytes).unwrap_or_else(|error| panic!("{name}: {error:?}")).pages.before;
    let added = |edits: Vec<Edit>| run(bytes, &marking(bytes, edits)).unwrap_or_else(|error| panic!("{name}: {error:?}"));
    let only = |case: &str, inspection: &Inspection, annotations: AnnotationChanges, pages: PageChanges| {
        assert!(inspection.base_kept, "{name}, {case}");
        assert_eq!(inspection.annotations, annotations, "{name}, {case}: {inspection:?}");
        assert_eq!(inspection.pages, pages, "{name}, {case}: {inspection:?}");
        assert_eq!((inspection.content.pages_with_content_changed, inspection.form_fields_changed), (0, 0), "{name}, {case}: {inspection:?}");
        assert!(inspection.info.changed.is_empty() && inspection.marks_with_overlay == 0, "{name}, {case}: {inspection:?}");
    };
    let same = PageChanges { before: count, after: count, ..PageChanges::default() };
    only(
        "a stamp",
        &added(vec![shape(0, Shape::Stamp { rect: [20.0, 20.0, 120.0, 60.0], stamp: pdfcraft_annot::StampKind::Approved, by: None })]),
        AnnotationChanges { stamps: 1, ..AnnotationChanges::default() },
        same,
    );
    for (case, drawing) in [
        ("ink", Shape::Ink { strokes: vec![vec![[20.0, 20.0], [60.0, 40.0], [90.0, 25.0]]] }),
        ("a rectangle", Shape::Rectangle { rect: [20.0, 20.0, 80.0, 60.0] }),
        ("an oval", Shape::Oval { rect: [20.0, 20.0, 80.0, 60.0] }),
        ("a line", Shape::Line { from: [20.0, 20.0], to: [90.0, 60.0], arrow: true }),
        ("a polygon", Shape::Polygon { vertices: vec![[20.0, 20.0], [80.0, 20.0], [50.0, 70.0]], cloud: false }),
        ("a cloud", Shape::Polygon { vertices: vec![[20.0, 20.0], [80.0, 20.0], [50.0, 70.0]], cloud: true }),
        ("a polyline", Shape::PolyLine { vertices: vec![[20.0, 20.0], [50.0, 50.0], [80.0, 20.0]] }),
    ] {
        only(case, &added(vec![shape(0, drawing)]), AnnotationChanges { drawings: 1, ..AnnotationChanges::default() }, same);
    }
    let marked =
        added(vec![shape(0, Shape::Redact { quads: vec![rect_quad([20.0, 20.0, 80.0, 40.0])], overlay: String::new(), look: Default::default() })]);
    only("a pending mark", &marked, AnnotationChanges { redaction_marks: 1, ..AnnotationChanges::default() }, same);
    assert_eq!(marked.marked_areas.len(), 1, "{name}");
    let inserted = added(vec![Edit::InsertBlankPage { at: count as usize, width: 595.0, height: 842.0 }]);
    assert_eq!(inserted.pages, PageChanges { before: count, after: count + 1, inserted: 1, ..PageChanges::default() }, "{name}: {inserted:?}");
    let mut line = format!("{name}: tools accepted (stamp, 7 drawings, a mark, a page inserted");
    if count >= 2 {
        let deleted = added(vec![
            shape(1, Shape::Stamp { rect: [20.0, 20.0, 120.0, 60.0], stamp: pdfcraft_annot::StampKind::Draft, by: None }),
            Edit::DeletePages { pages: vec![0] },
        ]);
        assert_eq!((deleted.annotations.stamps, deleted.pages.deleted), (1, 1), "{name}: {deleted:?}");
        let moved = added(vec![
            Edit::MovePages { pages: vec![count as usize - 1], to: 0 },
            shape(0, Shape::Ink { strokes: vec![vec![[20.0, 20.0], [60.0, 40.0]]] }),
        ]);
        assert_eq!(moved.annotations.drawings, 1, "{name}: {moved:?}");
        assert!(moved.pages.moved >= 1, "{name}: {moved:?}");
        line.push_str(", a stamp then a delete, a move then ink");
    }
    // A text field filled.
    let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
    if let Some(field) = pdfcraft_forms::fields(&doc).into_iter().find(|field| field.kind == pdfcraft_forms::FieldKind::Text) {
        let filled = added(vec![Edit::SetFieldValue { name: field.name.clone(), value: pdfcraft_forms::FieldValue::Text("Filled".into()) }]);
        assert_eq!(filled.form_fields_changed, 1, "{name}: {filled:?}");
        line.push_str(", a field filled");
    }
    line.push(')');
    // A line marked in the browser and applied on the server.
    if let Some(mut redaction) = redact_line(bytes, 0) {
        redaction.retain(|edit| !matches!(edit, Edit::ApplyRedactions { .. }));
        let edited = marking(bytes, redaction);
        match apply_marks(bytes, &edited, InspectLimits::default()) {
            Ok(applied) => {
                assert_eq!(applied.inspection.annotations.redactions_applied, 1, "{name}: {:?}", applied.inspection);
                line.push_str(&format!("; a line marked and applied on the server: accepted ({} glyphs)", applied.facts.glyphs));
            }
            Err(error) => panic!("{name}, a line marked and applied on the server: {error:?}"),
        }
    }
    line
}

#[test]
fn the_repository_samples_inspect_clean_and_after_ordinary_edits() {
    for (name, bytes) in REPOSITORY_SAMPLES {
        check(name, bytes);
        check_tools(name, bytes);
    }
}

/// Every `.pdf` in `PDFCRAFT_INSPECT_SAMPLES`: see the module documentation.
#[test]
#[ignore = "reads the PDFs of the directory PDFCRAFT_INSPECT_SAMPLES names"]
fn samples_in_a_directory() {
    let Ok(dir) = std::env::var("PDFCRAFT_INSPECT_SAMPLES") else { panic!("set PDFCRAFT_INSPECT_SAMPLES to a directory of PDFs") };
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("pdf")))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no PDFs in {dir}");
    for path in paths {
        let bytes = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        println!("{}", check(&name, &bytes));
        println!("{}", check_tools(&name, &bytes));
    }
}
