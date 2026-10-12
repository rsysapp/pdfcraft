//! Embed build: edit mode, driven through the host bridge (embed API 0.2).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod embed_support;

use std::sync::Arc;

use egui::{Key, Modifiers};
use pdfcraft_engine::{Edit, Markup, NewAnnotation, Shape, Style, rect_quad};
use pdfcraft_ui_egui::embed::{
    EditAuthor, EditPermissions, EditorMode, HostCommand, HostEvent, JumpTarget, MAX_OPEN_BYTES, Mark, ModeRequest, OPEN_TOO_LARGE, refusal,
};

fn permissions(all: bool) -> EditPermissions {
    EditPermissions { accept_reject_changes: false, organize_pages: all, fill_forms: all, redact: all, stamps: all, drawing: all }
}

fn edit_request_as(name: &str, date: &str, all: bool) -> HostCommand {
    HostCommand::SetMode(ModeRequest::Edit {
        author: EditAuthor { id: format!("u-{name}"), name: name.into() },
        revision_date: date.into(),
        permissions: permissions(all),
    })
}

fn edit_request(all: bool) -> HostCommand {
    edit_request_as("Sari Wulan", "2026-10-10T08:00:00Z", all)
}

fn annotation(shape: Shape) -> Edit {
    Edit::AddAnnotation(NewAnnotation {
        page: 0,
        style: Style::default_for(&shape),
        shape,
        contents: String::new(),
        author: "typed by the page".into(),
    })
}

fn ink(at: f64) -> Edit {
    annotation(Shape::Ink { strokes: vec![vec![[at, at], [at + 50.0, at + 50.0]]] })
}

fn editing(all: bool) -> (egui_kittest::Harness<'static, pdfcraft_ui_egui::PdfCraftApp>, pdfcraft_ui_egui::embed::HostBridge) {
    let (mut h, bridge) = embed_support::embedded();
    embed_support::open(&mut h, &bridge, "lease.pdf", embed_support::three_pages());
    bridge.send(edit_request(all));
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::ModeChanged { mode: EditorMode::Edit }));
    (h, bridge)
}

fn selection_events(events: &[HostEvent]) -> Vec<HostEvent> {
    events.iter().filter(|event| matches!(event, HostEvent::Selection { .. } | HostEvent::SelectionCleared)).cloned().collect()
}

fn refusals(events: &[HostEvent]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::EditRefused { code } => Some(*code),
            _ => None,
        })
        .collect()
}

#[test]
fn switches_to_edit_mode_and_back() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().execute("page.rotate"));
    bridge.send(HostCommand::SetMode(ModeRequest::View));
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::ModeChanged { mode: EditorMode::View }));
    assert!(!h.state_mut().execute("page.rotate"));
    assert!(refusals(&bridge.take_events()).is_empty(), "view mode refuses silently");
}

#[test]
fn stays_in_view_mode_without_an_open_document() {
    let (mut h, bridge) = embed_support::embedded();
    bridge.send(edit_request(true));
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::ModeChanged { mode: EditorMode::View }));
}

#[test]
fn opening_a_document_returns_to_view_mode_first() {
    let (mut h, bridge) = editing(true);
    bridge.send(HostCommand::Open { name: "other.pdf".into(), bytes: embed_support::three_pages() });
    h.run_steps(3);
    let events = bridge.take_events();
    assert_eq!(events.first(), Some(&HostEvent::ModeChanged { mode: EditorMode::View }), "{events:?}");
    assert!(events.iter().any(|event| matches!(event, HostEvent::Opened { .. })), "{events:?}");
}

// X8 (D6): an open refused before parsing is an open like any other.
#[test]
fn a_too_large_open_leaves_edit_mode_and_leaves_nothing_to_edit() {
    let (mut h, bridge) = editing(true);
    bridge.send(HostCommand::OpenTooLarge { len: MAX_OPEN_BYTES + 1 });
    h.run_steps(2);
    assert_eq!(
        bridge.take_events(),
        vec![HostEvent::ModeChanged { mode: EditorMode::View }, HostEvent::OpenFailed { message: OPEN_TOO_LARGE.into() }]
    );
    bridge.send(edit_request(true));
    h.run_steps(2);
    assert_eq!(bridge.take_events(), vec![HostEvent::ModeChanged { mode: EditorMode::View }]);
}

#[test]
fn opens_only_the_tools_the_permissions_allow() {
    let (mut h, bridge) = editing(false);
    for command in ["page.rotate", "page.delete", "page.organize", "redact.mark", "comment.stamp", "comment.ink", "form.clear"] {
        assert!(!h.state_mut().execute(command), "{command}");
    }
    assert_eq!(refusals(&bridge.take_events()), vec![refusal::COMMAND_NOT_ALLOWED; 7], "each refusal told once");
    let (mut all, _bridge) = editing(true);
    for command in ["page.rotate", "page.organize", "redact.mark", "comment.ink"] {
        assert!(all.state_mut().execute(command), "{command}");
    }
}

#[test]
fn never_offers_signing_protection_splitting_applying_redactions_or_new_documents() {
    let (mut h, bridge) = editing(true);
    let refused = [
        "sign.digital",
        "sign.certify",
        "protect.password",
        "protect.remove",
        "page.split",
        "page.extract",
        "page.combine",
        "file.open",
        "print.dialog",
        "form.flatten",
        "redact.apply",
        "redact.clear",
        "redact.sanitize",
        "file.save_as",
        "edit.text",
    ];
    for command in refused {
        assert!(!h.state_mut().execute(command), "{command}");
    }
    assert_eq!(refusals(&bridge.take_events()).len(), refused.len());
    // Display and navigation are no refused edits (W3-1).
    assert!(!h.state_mut().execute("view.full_screen"));
    assert!(!h.state_mut().execute("no.such.command"));
    assert_eq!(refusals(&bridge.take_events()), vec![refusal::UNKNOWN_COMMAND]);
}

// D8: the host's author and date, not the page's text or the clock.
#[test]
fn applies_drawings_in_the_host_s_author_s_name_and_date_and_refuses_comments_in_the_file() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().apply_edit(ink(10.0)));
    let added = h
        .state()
        .active_document()
        .and_then(|document| document.info.annotations.iter().find(|annotation| annotation.subtype == "Ink").cloned())
        .unwrap();
    assert_eq!(added.author.as_deref(), Some("Sari Wulan"));
    assert_eq!(added.modified.as_deref(), Some("2026-10-10 08:00"), "the host's date, as the page view shows it");
    bridge.take_events();
    let highlight = Shape::TextMarkup { kind: Markup::Highlight, quads: vec![rect_quad([20.0, 140.0, 100.0, 180.0])] };
    assert!(!h.state_mut().apply_edit(annotation(highlight)));
    assert_eq!(refusals(&bridge.take_events()), vec![refusal::COMMAND_NOT_ALLOWED]);
}

// D24: the file's own markup, anyone's, stays as it was.
#[test]
fn moves_and_deletes_only_annotations_made_in_this_edit_session() {
    let base = {
        let mut session = pdfcraft_engine::Session::new();
        let id = session.open("lease.pdf", None, Arc::new(embed_support::three_pages()), None).unwrap();
        let ink = Shape::Ink { strokes: vec![vec![[10.0, 10.0], [60.0, 60.0]]] };
        session
            .apply(
                id,
                Edit::AddAnnotation(NewAnnotation {
                    page: 0,
                    style: Style::default_for(&ink),
                    shape: ink,
                    contents: String::new(),
                    author: "Andi Pratama".into(),
                }),
            )
            .unwrap();
        session.save_bytes(id).unwrap().as_ref().clone()
    };
    let (mut h, bridge) = embed_support::embedded();
    embed_support::open(&mut h, &bridge, "lease.pdf", base);
    bridge.send(edit_request(true));
    h.run_steps(2);
    assert!(!h.state_mut().apply_edit(Edit::DeleteAnnotation { page: 0, index: 0 }), "the file's own drawing stays");
    assert!(!h.state_mut().apply_edit(Edit::MoveAnnotation { page: 0, index: 0, dx: 5.0, dy: 5.0 }));
    assert!(h.state_mut().apply_edit(ink(80.0)));
    let mine = h
        .state()
        .active_document()
        .and_then(|document| {
            document
                .info
                .annotations
                .iter()
                .filter(|annotation| annotation.page == 0 && annotation.subtype == "Ink")
                .map(|annotation| annotation.index)
                .max()
        })
        .unwrap();
    assert!(mine > 0);
    assert!(h.state_mut().apply_edit(Edit::MoveAnnotation { page: 0, index: mine, dx: 5.0, dy: 5.0 }));
    assert!(h.state_mut().apply_edit(Edit::DeleteAnnotation { page: 0, index: mine }));
}

// D20: a control in view mode, so an empty list in edit mode means hidden, not never found.
#[test]
fn hides_host_marks_while_editing_and_shows_them_again_in_view_mode() {
    let (mut h, bridge) = embed_support::embedded();
    embed_support::open(&mut h, &bridge, "lease.pdf", embed_support::three_pages());
    bridge.send(HostCommand::SetMarks(vec![Mark { id: "m".into(), text: "Definitions".into(), page: Some(1), color: egui::Color32::YELLOW }]));
    embed_support::settle(&mut h);
    assert_eq!(h.state().host_mark_rects().len(), 1, "the mark is painted in view mode");
    bridge.send(edit_request(true));
    h.run_steps(2);
    assert!(h.state().host_mark_rects().is_empty());
    bridge.send(HostCommand::SetMode(ModeRequest::View));
    embed_support::settle(&mut h);
    assert_eq!(h.state().host_mark_rects().len(), 1);
}

// D9: a selection reported in view mode is cleared before edit mode starts; none is reported
// while editing, nor on leaving for the one made while editing.
#[test]
fn clears_a_reported_selection_before_editing_and_reports_none_while_editing() {
    let (mut h, bridge) = embed_support::embedded();
    embed_support::open(&mut h, &bridge, "lease.pdf", embed_support::three_pages());
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert!(bridge.take_events().iter().any(|event| matches!(event, HostEvent::Selection { .. })));
    bridge.send(edit_request(true));
    h.run_steps(2);
    let events = bridge.take_events();
    let order: Vec<HostEvent> =
        events.iter().filter(|event| matches!(event, HostEvent::SelectionCleared | HostEvent::ModeChanged { .. })).cloned().collect();
    assert_eq!(order, vec![HostEvent::SelectionCleared, HostEvent::ModeChanged { mode: EditorMode::Edit }]);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert!(selection_events(&bridge.take_events()).is_empty());
    bridge.send(HostCommand::SetMode(ModeRequest::View));
    h.run_steps(3);
    assert!(selection_events(&bridge.take_events()).is_empty());
}

// D7: the page indicator follows the view in edit mode too.
#[test]
fn reports_page_changes_while_editing() {
    let (mut h, bridge) = editing(true);
    bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 3 }));
    h.run_steps(3);
    assert!(bridge.take_events().contains(&HostEvent::PageChanged { page: 3 }));
}

#[test]
fn refuses_a_mode_request_that_is_not_well_formed() {
    use pdfcraft_ui_egui::embed::parse_mode_request;
    let good = r#"{"mode":"edit","author":{"id":"u","name":"Sari","email":"ignored"},"revisionDate":"2026-10-10T08:00:00.123Z","permissions":{"acceptRejectChanges":false,"organizePages":true,"fillForms":true,"redact":true,"stamps":true,"drawing":true}}"#;
    assert!(matches!(parse_mode_request(good), Ok(ModeRequest::Edit { .. })), "unread author fields are ignored");
    assert_eq!(parse_mode_request(r#"{"mode":"view"}"#), Ok(ModeRequest::View));
    let long_name = format!(r#""name":"{}""#, "x".repeat(256));
    for bad in [
        r#"{"mode":"place-signature"}"#.to_string(),
        good.replace("2026-10-10T08:00:00.123Z", "today"),
        good.replace("2026-10-10T08:00:00.123Z", "2026-13-10T08:00:00Z"),
        good.replace("2026-10-10T08:00:00.123Z", "2026-02-31T08:00:00Z"),
        good.replace("2026-10-10T08:00:00.123Z", "2026-10-10T08:00:00+07:00"),
        good.replace(r#""redact":true"#, r#""redact":1"#),
        good.replace(r#""redact":true,"#, ""),
        good.replace(r#""drawing":true"#, r#""drawing":true,"signing":true"#),
        good.replace(r#""name":"Sari""#, r#""name":"  ""#),
        good.replace(r#""name":"Sari""#, &long_name),
        good.replace(r#""author":{"id":"u","name":"Sari","email":"ignored"},"#, ""),
    ] {
        assert!(parse_mode_request(&bad).is_err(), "{bad}");
    }
}

fn saved(events: &[HostEvent]) -> Option<(String, Vec<u8>)> {
    events.iter().find_map(|event| match event {
        HostEvent::SaveRequested { file_name, bytes } => Some((file_name.clone(), bytes.clone())),
        _ => None,
    })
}

#[test]
fn saves_an_appended_update_under_the_opened_name() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().apply_edit(Edit::RotatePages { pages: vec![0], degrees: 90 }));
    bridge.send(HostCommand::RequestSave);
    h.run_steps(2);
    let (file_name, bytes) = saved(&bridge.take_events()).expect("a save answer");
    assert_eq!(file_name, "lease.pdf");
    let original = embed_support::three_pages();
    assert_eq!(&bytes[..original.len()], &original[..], "the opened bytes start the saved file");
}

#[test]
fn takes_the_save_shortcut_as_a_save_request_in_edit_mode_only() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().execute("file.save"));
    h.run_steps(2);
    assert!(saved(&bridge.take_events()).is_some());
    bridge.send(HostCommand::SetMode(ModeRequest::View));
    h.run_steps(2);
    bridge.take_events();
    assert!(!h.state_mut().execute("file.save"));
    h.run_steps(2);
    assert!(
        bridge.take_events().iter().all(|event| !matches!(event, HostEvent::SaveRequested { .. } | HostEvent::SaveFailed { .. })),
        "nothing in view mode"
    );
}

#[test]
fn says_there_is_nothing_to_save_in_view_mode() {
    let (mut h, bridge) = embed_support::embedded();
    embed_support::open(&mut h, &bridge, "lease.pdf", embed_support::three_pages());
    bridge.send(HostCommand::RequestSave);
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::SaveFailed { message: "Nothing to save: the document is open for viewing only.".into() }));
}

// W3-2: a save holds the document and the mode as they were at the request.
#[test]
fn answers_a_save_from_the_state_at_its_request() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().apply_edit(Edit::RotatePages { pages: vec![0], degrees: 90 }));
    bridge.send(HostCommand::RequestSave);
    bridge.send(HostCommand::Open { name: "other.pdf".into(), bytes: embed_support::pages(&[&["Other"]]) });
    h.run_steps(3);
    let events = bridge.take_events();
    let (file_name, bytes) = saved(&events).expect("the save was answered");
    assert_eq!(file_name, "lease.pdf");
    assert!(bytes.len() > embed_support::three_pages().len(), "the edited lease, not the other file");

    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().apply_edit(Edit::RotatePages { pages: vec![0], degrees: 90 }));
    bridge.send(HostCommand::RequestSave);
    bridge.send(HostCommand::SetMode(ModeRequest::View));
    bridge.send(HostCommand::RequestSave);
    h.run_steps(2);
    let events = bridge.take_events();
    assert!(saved(&events).is_some(), "{events:?}");
    assert!(events.contains(&HostEvent::SaveFailed { message: "Nothing to save: the document is open for viewing only.".into() }));

    // Two requests in one frame for the same state answer once.
    let (mut h, bridge) = editing(true);
    bridge.send(HostCommand::RequestSave);
    bridge.send(HostCommand::RequestSave);
    h.run_steps(2);
    assert_eq!(bridge.take_events().iter().filter(|event| matches!(event, HostEvent::SaveRequested { .. })).count(), 1);
}

// R6b-49: marks only; their count goes to the host whenever it changes; nothing is applied here.
#[test]
fn marks_redactions_tells_the_host_how_many_and_never_applies_them() {
    let (mut h, bridge) = editing(true);
    let mark = Shape::Redact { quads: vec![rect_quad([80.0, 140.0, 100.0, 180.0])], overlay: "CONFIDENTIAL".into(), look: Default::default() };
    assert!(h.state_mut().apply_edit(annotation(mark)));
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::RedactionsPending { marks: 1 }));
    assert!(!h.state_mut().execute("redact.apply"));
    bridge.send(HostCommand::ApplyRedactions);
    h.run_steps(2);
    assert_eq!(h.state().active_document().map(|document| document.redaction_marks()), Some(1), "applyRedactions does nothing");
    bridge.send(HostCommand::RequestSave);
    h.run_steps(2);
    let (_, bytes) = saved(&bridge.take_events()).expect("a save answer");
    assert!(bytes.starts_with(&embed_support::three_pages()), "an appended update with the mark");
    assert!(h.state_mut().execute("edit.undo"));
    h.run_steps(2);
    let events = bridge.take_events();
    assert!(events.contains(&HostEvent::RedactionsPending { marks: 0 }), "{events:?}");
}

#[test]
fn reports_dirty_after_an_edit_and_clean_again_after_undoing_it() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().apply_edit(Edit::RotatePages { pages: vec![0], degrees: 90 }));
    h.run_steps(2);
    assert_eq!(
        bridge.take_events().iter().filter(|event| matches!(event, HostEvent::DirtyChanged { .. })).collect::<Vec<_>>(),
        vec![&HostEvent::DirtyChanged { dirty: true }]
    );
    // A save never clears dirty (W3-4).
    bridge.send(HostCommand::RequestSave);
    h.run_steps(2);
    assert!(!bridge.take_events().iter().any(|event| matches!(event, HostEvent::DirtyChanged { .. })));
    assert!(h.state_mut().execute("edit.undo"));
    h.run_steps(2);
    assert!(bridge.take_events().contains(&HostEvent::DirtyChanged { dirty: false }));
}

#[test]
fn turns_a_comment_tool_into_nothing_without_a_selection() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().execute("comment.highlight"));
    h.run_steps(2);
    assert!(!bridge.take_events().iter().any(|event| matches!(event, HostEvent::CommentRequested { .. })));
    // No tool was taken up and nothing was written into the file.
    assert_eq!(h.state().active_document().map(|document| document.info.annotations.len()), Some(0));
}

#[test]
fn turns_a_comment_tool_with_a_selection_into_a_comment_request() {
    let (mut h, bridge) = editing(true);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    for command in ["comment.highlight", "comment.note", "comment.underline"] {
        assert!(h.state_mut().execute(command));
        h.run_steps(2);
        let events = bridge.take_events();
        let request = events.iter().find(|event| matches!(event, HostEvent::CommentRequested { .. }));
        assert!(
            matches!(request, Some(HostEvent::CommentRequested { text, page: Some(_), .. }) if text.contains("Definitions")),
            "{command}: {events:?}"
        );
    }
    assert_eq!(h.state().active_document().map(|document| document.info.annotations.len()), Some(0));
}

// W3-5: one author and one date per save.
#[test]
fn keeps_one_author_and_one_date_per_save() {
    let (mut h, bridge) = editing(true);
    assert!(h.state_mut().apply_edit(ink(10.0)));
    h.run_steps(2);
    bridge.take_events();
    for other in [edit_request_as("Sari Wulan", "2026-10-11T08:00:00Z", true), edit_request_as("Budi", "2026-10-10T08:00:00Z", true)] {
        bridge.send(other);
        h.run_steps(2);
        assert_eq!(
            bridge.take_events(),
            vec![HostEvent::EditRefused { code: refusal::INVALID_EDIT_SESSION }, HostEvent::ModeChanged { mode: EditorMode::Edit }]
        );
    }
    // The same author and date continue the session, from view mode too.
    bridge.send(HostCommand::SetMode(ModeRequest::View));
    bridge.send(edit_request(true));
    h.run_steps(2);
    assert_eq!(bridge.take_events(), vec![HostEvent::ModeChanged { mode: EditorMode::View }, HostEvent::ModeChanged { mode: EditorMode::Edit }]);
    let mine = h
        .state()
        .active_document()
        .and_then(|document| document.info.annotations.iter().find(|annotation| annotation.subtype == "Ink").map(|annotation| annotation.index))
        .unwrap();
    assert!(h.state_mut().apply_edit(Edit::MoveAnnotation { page: 0, index: mine, dx: 5.0, dy: 5.0 }), "still this session's");
    // Undone back to the opened file, another date starts a new session, and redo brings nothing back.
    while h.state_mut().execute("edit.undo") {}
    h.run_steps(2);
    bridge.take_events();
    bridge.send(edit_request_as("Budi", "2026-10-11T08:00:00Z", true));
    h.run_steps(2);
    assert_eq!(bridge.take_events(), vec![HostEvent::ModeChanged { mode: EditorMode::Edit }]);
    h.state_mut().execute("edit.redo");
    assert_eq!(h.state().active_document().map(|document| document.info.annotations.len()), Some(0));
}
