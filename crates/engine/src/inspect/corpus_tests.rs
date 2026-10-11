//! Embed build: ordinary files are not refused. Every PDF sample in the repository inspects clean
//! against itself and after the edits a host accepts; a directory of other samples is checked the
//! same way on request:
//!
//! ```text
//! PDFCRAFT_INSPECT_SAMPLES=/path/to/pdfs cargo test -p pdfcraft-engine --lib \
//!     inspect::corpus_tests::samples_in_a_directory -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use super::*;
use crate::{Edit, Session};

/// The repository's PDF samples (`git ls-files '*.pdf'`).
const REPOSITORY_SAMPLES: [(&str, &[u8]); 2] = [
    ("crates/sign/tests/data/openssl-signed.pdf", include_bytes!("../../../sign/tests/data/openssl-signed.pdf")),
    ("crates/ui-egui/tests/data/form.pdf", include_bytes!("../../../ui-egui/tests/data/form.pdf")),
];

fn run(base: &[u8], edited: &[u8]) -> Result<Inspection, InspectError> {
    inspect(base, edited, InspectLimits::default())
}

fn edit(base: &[u8], edits: Vec<Edit>) -> Vec<u8> {
    let mut session = Session::new().with_clock(|| 1_700_000_000);
    let id = session.open("sample.pdf", None, Arc::new(base.to_vec()), None).unwrap();
    for edit in edits {
        session.apply(id, edit).unwrap();
    }
    session.save_bytes(id).unwrap().as_ref().clone()
}

/// Nothing changed but what `pages` says, the outline facts, and `removed` annotations (those
/// of a deleted page).
fn assert_only(name: &str, inspection: &Inspection, pages: PageChanges, removed: u32) {
    assert_eq!(inspection.pages, pages, "{name}: {inspection:?}");
    assert_eq!(inspection.content, ContentChanges::default(), "{name}: {inspection:?}");
    assert_eq!(inspection.text, TextChanges::default(), "{name}: {inspection:?}");
    assert_eq!(inspection.annotations, AnnotationChanges { removed, ..AnnotationChanges::default() }, "{name}: {inspection:?}");
    assert_eq!(inspection.form_fields_changed, 0, "{name}: {inspection:?}");
}

/// Check one sample: unchanged, rotated, and (with two pages or more) a page deleted and a page
/// moved. Returns a line describing what was found.
fn check(name: &str, bytes: &[u8]) -> String {
    let inspection = run(bytes, bytes).unwrap_or_else(|error| panic!("{name}, unchanged: {error:?}"));
    let count = inspection.pages.before;
    assert!(inspection.base_kept, "{name}");
    assert_only(name, &inspection, PageChanges { before: count, after: count, ..PageChanges::default() }, 0);
    assert_eq!(inspection.outlines, OutlineChanges::default(), "{name}");
    let rotated = run(bytes, &edit(bytes, vec![Edit::RotatePages { pages: vec![0], degrees: 90 }]))
        .unwrap_or_else(|error| panic!("{name}, rotated: {error:?}"));
    assert_only(name, &rotated, PageChanges { before: count, after: count, rotated: 1, ..PageChanges::default() }, 0);
    assert_eq!(rotated.outlines, OutlineChanges::default(), "{name}, rotated");
    let mut line = format!("{name}: {count} page(s), clean; rotated: clean");
    if count >= 2 {
        let last = count as usize - 1;
        let deleted = run(bytes, &edit(bytes, vec![Edit::DeletePages { pages: vec![0] }]))
            .unwrap_or_else(|error| panic!("{name}, first page deleted: {error:?}"));
        // The deleted page's annotations went with it.
        let annotations = deleted.annotations.removed;
        assert_only(name, &deleted, PageChanges { before: count, after: count - 1, deleted: 1, ..PageChanges::default() }, annotations);
        assert_eq!(deleted.outlines.titles_changed + deleted.outlines.added + deleted.outlines.removed, 0, "{name}: {deleted:?}");
        let moved = run(bytes, &edit(bytes, vec![Edit::MovePages { pages: vec![last], to: 0 }]))
            .unwrap_or_else(|error| panic!("{name}, last page moved first: {error:?}"));
        assert_eq!(moved.pages.deleted + moved.pages.inserted, 0, "{name}: {moved:?}");
        assert!(moved.pages.moved >= 1, "{name}: {moved:?}");
        assert_eq!(moved.content, ContentChanges::default(), "{name}: {moved:?}");
        assert_eq!(moved.text, TextChanges::default(), "{name}: {moved:?}");
        assert_eq!(moved.outlines.titles_changed + moved.outlines.added + moved.outlines.removed, 0, "{name}: {moved:?}");
        line.push_str(&format!(
            "; first page deleted: accepted ({annotations} annotation(s) gone with it), {:?}; last page moved first: accepted ({} moved), {:?}",
            deleted.outlines, moved.pages.moved, moved.outlines
        ));
    }
    line
}

#[test]
fn the_repository_samples_inspect_clean_and_after_ordinary_edits() {
    for (name, bytes) in REPOSITORY_SAMPLES {
        check(name, bytes);
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
    }
}
