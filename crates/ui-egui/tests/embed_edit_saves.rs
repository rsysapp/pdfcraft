//! Embed build: every tool edit mode offers makes a save the host takes. Each test opens a file
//! through the host bridge, edits in edit mode, saves, and checks the save with the inspector the
//! host runs (`pdfcraft_engine::inspect`), judged by the host's rules for a browser's save.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod embed_support;

use egui::{Key, Modifiers};
use pdfcraft_engine::inspect::{InspectLimits, Inspection, apply_marks, inspect};
use pdfcraft_engine::{Edit, NewAnnotation, Shape, Style};
use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::embed::{EditAuthor, EditPermissions, EditorMode, HostBridge, HostCommand, HostEvent, ModeRequest};

type Harness = egui_kittest::Harness<'static, PdfCraftApp>;

const ALL: EditPermissions =
    EditPermissions { accept_reject_changes: false, organize_pages: true, fill_forms: true, redact: true, stamps: true, drawing: true };

fn editing(bytes: Vec<u8>) -> (Harness, HostBridge) {
    let (mut h, bridge) = embed_support::embedded();
    embed_support::open(&mut h, &bridge, "lease.pdf", bytes);
    bridge.send(HostCommand::SetMode(ModeRequest::Edit {
        author: EditAuthor { id: "u-sari".into(), name: "Sari Wulan".into() },
        revision_date: "2026-10-10T08:00:00Z".into(),
        permissions: ALL,
    }));
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::ModeChanged { mode: EditorMode::Edit }));
    (h, bridge)
}

fn save(h: &mut Harness, bridge: &HostBridge) -> Vec<u8> {
    bridge.send(HostCommand::RequestSave);
    h.run_steps(2);
    bridge
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            HostEvent::SaveRequested { bytes, .. } => Some(bytes),
            _ => None,
        })
        .expect("a save answer")
}

/// The host's rules for a browser's save (`judgePdfSave` and the appended-update rules), as the
/// inspection's facts show them: `Err` names the refusal.
fn host_judges(inspection: &Inspection, permissions: EditPermissions) -> Result<(), &'static str> {
    let (pages, annotations, content, text, outlines) =
        (&inspection.pages, &inspection.annotations, &inspection.content, &inspection.text, &inspection.outlines);
    let removed = &inspection.structure_removed;
    let page_edits = pages.rotated + pages.inserted + pages.deleted + pages.moved;
    let renumbered = pages.inserted + pages.deleted + pages.moved > 0;
    let checks: [(bool, &'static str); 15] = [
        (!inspection.base_kept, "rewritten_in_browser"),
        (annotations.comments > 0, "comments_in_file"),
        (!inspection.info.changed.is_empty(), "info_changed"),
        (text.pages_with_text_removed + text.pages_with_text_added > 0, "text_changed"),
        (
            content.pages_with_content_removed
                + content.pages_with_images_removed
                + content.pages_with_images_added
                + content.pages_with_content_changed
                + content.pages_changed_outside_redactions
                + removed.tagged_text
                + removed.metadata
                + removed.actions
                + removed.thumbnails
                + removed.page_entries
                > 0,
            "content_changed",
        ),
        (annotations.removed + annotations.changed > 0, "annotations_changed"),
        (outlines.titles_changed + outlines.added + outlines.removed > 0, "outline_changed"),
        (outlines.destinations_changed + inspection.destinations.named + inspection.destinations.links > 0 && !renumbered, "destinations_changed"),
        (inspection.destinations.renamed > 0, "destinations_changed"),
        (inspection.labels.followed > 0 && !renumbered, "labels_changed"),
        (annotations.other > 0, "annotation_not_allowed"),
        (inspection.marks_with_overlay > 0, "redaction_overlay"),
        (page_edits > 0 && !permissions.organize_pages, "pages_not_allowed"),
        (inspection.form_fields_changed > 0 && !permissions.fill_forms, "forms_not_allowed"),
        (page_edits + inspection.form_fields_changed + annotations.stamps + annotations.drawings + annotations.redaction_marks == 0, "no_changes"),
    ];
    match checks.iter().find(|(refused, _)| *refused) {
        Some((_, code)) => Err(code),
        None => Ok(()),
    }
}

/// Inspect a save against its base and judge it as the host does.
fn accepted(base: &[u8], saved: &[u8]) -> Inspection {
    let inspection = inspect(base, saved, InspectLimits::default()).expect("the host's inspector reads the save");
    assert_eq!(host_judges(&inspection, ALL), Ok(()), "{inspection:#?}");
    inspection
}

fn shape_edit(shape: Shape) -> Edit {
    Edit::AddAnnotation(NewAnnotation { page: 0, style: Style::default_for(&shape), shape, contents: String::new(), author: String::new() })
}

#[test]
fn a_drawing_is_accepted() {
    let base = embed_support::three_pages();
    let (mut h, bridge) = editing(base.clone());
    assert!(h.state_mut().apply_edit(shape_edit(Shape::Ink { strokes: vec![vec![[10.0, 10.0], [60.0, 60.0]]] })));
    assert!(h.state_mut().apply_edit(shape_edit(Shape::Rectangle { rect: [100.0, 100.0, 160.0, 140.0] })));
    let inspection = accepted(&base, &save(&mut h, &bridge));
    assert_eq!(inspection.annotations.drawings, 2);
}

#[test]
fn a_stamp_is_accepted() {
    let base = embed_support::three_pages();
    let (mut h, bridge) = editing(base.clone());
    let stamp = Shape::Stamp { rect: [200.0, 200.0, 320.0, 240.0], stamp: pdfcraft_engine::StampKind::Approved, by: None };
    assert!(h.state_mut().apply_edit(shape_edit(stamp)));
    assert_eq!(accepted(&base, &save(&mut h, &bridge)).annotations.stamps, 1);
}

#[test]
fn a_comment_goes_to_the_host_and_never_into_the_saved_file() {
    let base = embed_support::three_pages();
    let (mut h, bridge) = editing(base.clone());
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert!(h.state_mut().execute("comment.highlight"));
    h.run_steps(2);
    assert!(bridge.take_events().iter().any(|event| matches!(event, HostEvent::CommentRequested { .. })));
    assert!(h.state_mut().apply_edit(shape_edit(Shape::Ink { strokes: vec![vec![[10.0, 10.0], [60.0, 60.0]]] })));
    assert_eq!(accepted(&base, &save(&mut h, &bridge)).annotations.comments, 0);
}

#[test]
fn page_rotation_deletion_and_moves_are_accepted() {
    let base = embed_support::three_pages();
    let (mut h, bridge) = editing(base.clone());
    assert!(h.state_mut().execute("page.rotate"));
    assert!(h.state_mut().apply_edit(Edit::MovePages { pages: vec![2], to: 0 }));
    assert!(h.state_mut().apply_edit(Edit::DeletePages { pages: vec![2] }));
    let inspection = accepted(&base, &save(&mut h, &bridge));
    assert_eq!((inspection.pages.rotated, inspection.pages.deleted, inspection.pages.moved), (1, 1, 1), "{:?}", inspection.pages);
}

#[test]
fn inserted_pages_are_accepted() {
    let base = embed_support::three_pages();
    let (mut h, bridge) = editing(base.clone());
    assert!(h.state_mut().execute("page.insert_blank"));
    h.state_mut().insert_files(vec![("other.pdf".into(), embed_support::pages(&[&["Annex"]]))]);
    assert_eq!(h.state().active_document().map(|document| document.info.pages.len()), Some(5));
    assert_eq!(accepted(&base, &save(&mut h, &bridge)).pages.inserted, 2);
}

#[test]
fn a_filled_form_is_accepted() {
    let base = include_bytes!("data/form.pdf").to_vec();
    let (mut h, bridge) = editing(base.clone());
    let name = h
        .state()
        .active_document()
        .and_then(|document| document.form.iter().find(|field| field.kind == pdfcraft_engine::FormFieldKind::Text).map(|field| field.name.clone()))
        .expect("a text field");
    assert!(h.state_mut().apply_edit(Edit::SetFieldValue { name, value: pdfcraft_engine::FieldValue::Text("Sari Wulan".into()) }));
    assert!(accepted(&base, &save(&mut h, &bridge)).form_fields_changed > 0);
}

#[test]
fn a_redaction_mark_is_accepted_and_the_server_can_apply_it() {
    let base = embed_support::three_pages();
    let (mut h, bridge) = editing(base.clone());
    // A whole line selected, then the redaction tool marks it.
    let view = &mut h.state_mut().views[0];
    view.select_text(0, 0, "Definitions".len() - 1);
    assert!(h.state_mut().execute("redact.mark"));
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::RedactionsPending { marks: 1 }));
    let saved = save(&mut h, &bridge);
    let inspection = accepted(&base, &saved);
    assert_eq!((inspection.annotations.redaction_marks, inspection.marks_with_overlay), (1, 0));
    let applied = apply_marks(&base, &saved, InspectLimits::default()).expect("the server applies the mark");
    assert!(!applied.bytes.is_empty());
}

/// Three pages labelled i, ii, 1, with destinations by number and by reference: a named
/// destination to page 3 by number and one to page 2 by reference, a bookmark and a link to page 3
/// by number.
fn labelled() -> Vec<u8> {
    let content = |text: &str| {
        let body = format!("BT /F1 14 Tf 40 260 Td ({text}) Tj ET");
        format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len())
    };
    embed_support::write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels << /Nums [0 << /S /r >> 2 << /S /D >>] >> /Names << /Dests 9 0 R >> /Outlines 10 0 R >>".into(),
        "<< /Type /Pages /Kids [4 0 R 6 0 R 8 0 R] /Count 3 /MediaBox [0 0 400 300] >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into(),
        "<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> /Annots [12 0 R] >>".into(),
        content("Cover"),
        "<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources << /Font << /F1 3 0 R >> >> >>".into(),
        content("Contents"),
        "<< /Type /Page /Parent 2 0 R /Contents 13 0 R /Resources << /Font << /F1 3 0 R >> >> >>".into(),
        "<< /Names [(end) [2 /Fit] (intro) [6 0 R /Fit]] >>".into(),
        "<< /Type /Outlines /First 11 0 R /Last 11 0 R /Count 1 >>".into(),
        "<< /Title (Body) /Parent 10 0 R /Dest [2 /Fit] >>".into(),
        "<< /Type /Annot /Subtype /Link /Rect [10 10 50 30] /Border [0 0 0] /Dest [2 /Fit] >>".into(),
        content("Body"),
    ])
}

#[test]
fn labels_and_destinations_follow_their_pages() {
    let base = labelled();
    for edits in [
        vec![Edit::DeletePages { pages: vec![1] }],
        vec![Edit::MovePages { pages: vec![2], to: 0 }],
        vec![Edit::InsertBlankPage { at: 0, width: 400.0, height: 300.0 }],
        vec![Edit::DeletePages { pages: vec![1] }, Edit::MovePages { pages: vec![1], to: 0 }],
        vec![Edit::MovePages { pages: vec![2], to: 0 }, Edit::InsertBlankPage { at: 1, width: 400.0, height: 300.0 }],
    ] {
        let (mut h, bridge) = editing(base.clone());
        for edit in &edits {
            assert!(h.state_mut().apply_edit(edit.clone()), "{edit:?}");
        }
        let inspection = accepted(&base, &save(&mut h, &bridge));
        assert!(inspection.labels.followed > 0, "{edits:?}: {:?}", inspection.labels);
        assert!(inspection.destinations.named + inspection.destinations.links + inspection.outlines.destinations_changed > 0, "{edits:?}");
    }
}
