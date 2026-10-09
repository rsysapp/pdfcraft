//! The embed build's view profile and `open` (`PdfCraftApp::embedded`).

mod embed_support;

use egui_kittest::kittest::Queryable;
use embed_support::{embedded, open, pages, protected};
use pdfcraft_ui_egui::embed::HostEvent;

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
    assert!(matches!(events.as_slice(), [HostEvent::OpenFailed { message }] if message.starts_with("This file can't be shown")), "{events:?}");
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
