//! The embed build's view profile and `open` (`PdfCraftApp::embedded`).

mod embed_support;

use egui_kittest::kittest::Queryable;
use embed_support::{embedded, open, pages, protected, settle, three_pages};
use pdfcraft_ui_egui::embed::{HostCommand, HostEvent, JumpTarget, MAX_OPEN_BYTES, OPEN_DAMAGED, OPEN_TOO_LARGE, open_command};

#[test]
fn the_view_profile_shows_the_page_navigation_and_nothing_else() {
    let (mut h, bridge) = embedded();
    h.run_steps(3);
    assert!(h.query_by_label("Join the ArtCraft community").is_none(), "no home screen");
    let events = open(&mut h, &bridge, "two.pdf", pages(&[&["First page"], &["Second page"]]));
    assert_eq!(events.first(), Some(&HostEvent::Opened { pages: 2 }));
    for label in ["Find text", "Zoom in (⌘+)", "Zoom out (⌘−)", "Next page", "Previous page"] {
        h.get_by_label(label);
    }
    for label in ["Comments", "Bookmarks", "Select (V)", "Hand (H)", "Fill & Sign", "Discord", "two.pdf"] {
        assert!(h.query_by_label(label).is_none(), "{label} is not part of the view profile");
    }
    h.get_by_label("Next page").click();
    h.run_steps(3);
    assert_eq!(h.state().views[0].current, 1);
}

#[test]
fn opening_answers_opened_or_open_failed_and_never_prompts() {
    let (mut h, bridge) = embedded();
    let events = open(&mut h, &bridge, "broken.pdf", b"this is not a PDF".to_vec());
    // A plain, fixed message: the engine's error text goes to the log, never to the host.
    assert_eq!(events, [HostEvent::OpenFailed { message: OPEN_DAMAGED.into() }]);
    assert!(h.state().views.is_empty());
    let events = open(&mut h, &bridge, "locked.pdf", protected("secret"));
    assert_eq!(events, [HostEvent::OpenFailed { message: "This file is password-protected and can't be shown here.".into() }]);
    assert!(h.state().password_prompt.is_none() && h.state().views.is_empty());
    let first = open(&mut h, &bridge, "a.pdf", pages(&[&["Alpha"]]));
    assert_eq!(first.first(), Some(&HostEvent::Opened { pages: 1 }));
    let second = open(&mut h, &bridge, "b.pdf", pages(&[&["Beta"], &["Gamma"], &["Delta"]]));
    assert_eq!(second.first(), Some(&HostEvent::Opened { pages: 3 }));
    assert_eq!(h.state().views.len(), 1, "open replaces the document");
    assert!(h.state().session.view_only());
}

#[test]
fn an_open_over_the_size_cap_never_copies_its_bytes() {
    for len in [MAX_OPEN_BYTES + 1, usize::MAX] {
        let mut copied = false;
        let command = open_command("big.pdf".into(), len, || {
            copied = true;
            Ok::<_, ()>(Vec::new())
        });
        assert_eq!(command, Ok(HostCommand::OpenTooLarge { len }));
        assert!(!copied, "{len} bytes are refused before the copy");
    }
    let at_cap = open_command("cap.pdf".into(), MAX_OPEN_BYTES, || Ok::<_, ()>(b"%PDF-".to_vec()));
    assert_eq!(at_cap, Ok(HostCommand::Open { name: "cap.pdf".into(), bytes: b"%PDF-".to_vec() }));
}

#[test]
fn an_empty_open_is_never_read_and_a_failed_copy_queues_nothing() {
    // A detached or transferred buffer has length 0; reading it would throw.
    let empty = open_command("gone.pdf".into(), 0, || -> Result<Vec<u8>, &str> { panic!("an empty file is never read") });
    assert_eq!(empty, Ok(HostCommand::Open { name: "gone.pdf".into(), bytes: Vec::new() }));
    assert_eq!(open_command("a.pdf".into(), 5, || Err("detached")), Err("detached"));
}

#[test]
fn an_empty_open_is_answered_open_failed() {
    let (mut h, bridge) = embedded();
    let events = open(&mut h, &bridge, "gone.pdf", Vec::new());
    assert_eq!(events, [HostEvent::OpenFailed { message: OPEN_DAMAGED.into() }]);
}

#[test]
fn an_open_over_the_size_cap_is_answered_in_order_and_changes_nothing_else() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 2 }));
    h.run_steps(3);
    bridge.take_events();
    let too_large = HostEvent::OpenFailed { message: OPEN_TOO_LARGE.into() };
    assert_eq!(OPEN_TOO_LARGE, "This file is too large or complex to be shown here.", "the Word editor's words");

    let refused = open_command("huge.pdf".into(), MAX_OPEN_BYTES + 1, || Ok::<_, ()>(Vec::new())).expect("never copies");
    bridge.send(refused.clone());
    // A native caller that sends the bytes anyway hears the same answer.
    bridge.send(HostCommand::Open { name: "huge.pdf".into(), bytes: vec![0; MAX_OPEN_BYTES + 1] });
    h.run_steps(3);
    settle(&mut h);
    assert_eq!(bridge.take_events(), [too_large.clone(), too_large.clone()], "only the answers");
    assert_eq!(h.state().views.len(), 1, "the open document stays");
    assert_eq!(h.state().views[0].current, 1, "where the host left it");

    // In order among other answers: before the next open's, and the document that open brings stays.
    bridge.send(refused.clone());
    bridge.send(HostCommand::Open { name: "a.pdf".into(), bytes: pages(&[&["Alpha"]]) });
    bridge.send(refused);
    h.run_steps(3);
    settle(&mut h);
    assert_eq!(bridge.take_events(), [too_large.clone(), HostEvent::Opened { pages: 1 }, too_large, HostEvent::PageChanged { page: 1 }]);
    assert_eq!(h.state().views.len(), 1);
    assert!(!h.state().host_work_pending());
}

fn right_click_at(h: &mut egui_kittest::Harness<'static, pdfcraft_ui_egui::PdfCraftApp>, at: egui::Pos2) {
    h.hover_at(at);
    h.run_steps(2);
    h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed: true, modifiers: Default::default() });
    h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed: false, modifiers: Default::default() });
    h.run_steps(3);
}

#[test]
fn a_right_click_on_the_page_opens_no_menu_in_view_mode() {
    let bytes = pages(&[&["First page"]]);
    let at = egui::pos2(300.0, 350.0);

    // The same click in the full app opens the page menu, so the absence below means something.
    let page = bytes.clone();
    let mut full = egui_kittest::Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_eframe(move |_cc| {
        let mut app = pdfcraft_ui_egui::PdfCraftApp::new();
        app.open_bytes("one.pdf", None, page).expect("opens");
        app
    });
    full.run_steps(4);
    right_click_at(&mut full, at);
    full.get_by_label("Comments panel");

    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "one.pdf", bytes);
    right_click_at(&mut h, at);
    assert!(h.query_by_label("Comments panel").is_none(), "view mode has no page menu");
}

#[test]
fn the_page_display_menu_has_no_read_or_full_screen_mode_in_view_mode() {
    let bytes = pages(&[&["First page"], &["Second page"]]);

    // The full app's right rail offers both, so their absence below means something.
    let page = bytes.clone();
    let mut full = egui_kittest::Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_eframe(move |_cc| {
        let mut app = pdfcraft_ui_egui::PdfCraftApp::new();
        app.open_bytes("two.pdf", None, page).expect("opens");
        app
    });
    full.run_steps(4);
    full.get_by_label_contains("Page display:").click();
    full.run_steps(3);
    full.get_by_label("Read mode");
    full.get_by_label("Full screen mode");

    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "two.pdf", bytes);
    h.get_by_label_contains("Page display:").click();
    h.run_steps(3);
    h.get_by_label("Fit one full page");
    assert!(h.query_by_label("Read mode").is_none(), "Read mode is not part of the view profile");
    assert!(h.query_by_label("Full screen mode").is_none(), "Full screen mode is not part of the view profile");
}
