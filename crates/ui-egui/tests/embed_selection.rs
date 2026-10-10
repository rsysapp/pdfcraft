//! Selection events in the embed build: sent when a selection settles.

mod embed_support;

use egui::{Event, Key, Modifiers, PointerButton};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use embed_support::{embedded, open, pages, settle, three_pages};
use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::embed::{HostBridge, HostCommand, HostEvent, JumpTarget};

fn selections(events: &[HostEvent]) -> Vec<HostEvent> {
    events.iter().filter(|e| matches!(e, HostEvent::Selection { .. } | HostEvent::SelectionCleared)).cloned().collect()
}

fn two_lines() -> Vec<u8> {
    pages(&[&["The quick brown fox", "jumps over the dog"]])
}

#[test]
fn a_dragged_selection_is_reported_once_the_button_is_released() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "lines.pdf", two_lines());
    // Glyphs: "The" 0–2, a space 3, "quick" 4–8.
    let (a, b) = {
        let view = &h.state().views[0];
        (view.glyph_screen_pos(0, 4).expect("q on screen"), view.glyph_screen_pos(0, 8).expect("k on screen"))
    };
    h.hover_at(a);
    h.run_steps(1);
    h.event(Event::PointerButton { pos: a, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.run_steps(1);
    for k in 1..=4 {
        h.event(Event::PointerMoved(a + (b - a) * (k as f32 / 4.0)));
        h.run_steps(1);
    }
    assert!(selections(&bridge.take_events()).is_empty(), "nothing while the button is down");
    h.event(Event::PointerButton { pos: b, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
    let events = selections(&bridge.take_events());
    let [HostEvent::Selection { text, copy_text, page, x, y }] = events.as_slice() else { panic!("one selection event: {events:?}") };
    assert_eq!((text.as_str(), copy_text.as_str(), *page), ("quick", "quick", Some(1)));
    let page_rect = h.state().views[0].page_screen_rect(0).expect("on screen");
    assert!(*x >= b.x && *y >= b.y && page_rect.expand(1.0).contains(egui::pos2(*x, *y)), "({x}, {y}) ends the selection");
    h.run_steps(3);
    assert!(selections(&bridge.take_events()).is_empty(), "reported once");
}

#[test]
fn keyboard_selection_and_clearing_are_reported() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "lines.pdf", two_lines());
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    let events = selections(&bridge.take_events());
    let [HostEvent::Selection { text, copy_text, .. }] = events.as_slice() else { panic!("one selection event: {events:?}") };
    assert_eq!(copy_text, "The quick brown fox\njumps over the dog", "copyText keeps line breaks");
    assert_eq!(text, "The quick brown fox jumps over the dog", "text collapses them");
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert_eq!(selections(&bridge.take_events()), [HostEvent::SelectionCleared]);
    h.run_steps(2);
    assert!(selections(&bridge.take_events()).is_empty(), "cleared once");
}

/// Where Select all ends in CSS pixels, and the page's rectangle in egui points, with the zoom
/// factor set to `zoom` before the file opens.
fn select_all_end(zoom: f32) -> ((f32, f32), egui::Rect) {
    let (mut h, bridge) = embedded();
    h.ctx.set_zoom_factor(zoom);
    h.run_steps(2);
    open(&mut h, &bridge, "lines.pdf", two_lines());
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert_eq!(h.ctx.zoom_factor(), zoom);
    let events = selections(&bridge.take_events());
    let [HostEvent::Selection { x, y, .. }] = events.as_slice() else { panic!("one selection event: {events:?}") };
    assert!(x.is_finite() && y.is_finite() && *x > 0.0 && *y > 0.0, "({x}, {y}) is a real point");
    ((*x, *y), h.state().views[0].page_screen_rect(0).expect("on screen"))
}

#[test]
fn coordinates_are_css_pixels_so_they_scale_with_the_zoom_factor() {
    // egui lays out in points, a CSS pixel is 1/zoom of one: the same selection end, in points,
    // must come back multiplied by the factor.
    for zoom in [1.0_f32, 1.5] {
        let ((x, y), page_rect) = select_all_end(zoom);
        let in_points = egui::pos2(x / zoom, y / zoom);
        assert!(page_rect.expand(1.0).contains(in_points), "({x}, {y}) / {zoom} = {in_points:?} is not on the page {page_rect:?}");
    }
    let ((x1, _), rect1) = select_all_end(1.0);
    let ((x15, _), rect15) = select_all_end(1.5);
    // The page is laid out smaller in points at 1.5; the end stays at the same place on it only if
    // the factor was applied once.
    assert!(
        ((x15 / 1.5 - rect15.min.x) / rect15.width() - (x1 - rect1.min.x) / rect1.width()).abs() < 0.02,
        "same place on the page at both factors"
    );
}

#[test]
fn a_whitespace_only_selection_clears_the_one_reported() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "lines.pdf", two_lines());
    // Glyphs: "The" 0–2, a space 3, "quick" 4–8.
    h.state_mut().views[0].select_text(0, 4, 8);
    h.run_steps(2);
    let events = selections(&bridge.take_events());
    assert!(matches!(events.as_slice(), [HostEvent::Selection { text, .. }] if text == "quick"), "{events:?}");
    h.state_mut().views[0].select_text(0, 3, 3);
    h.run_steps(2);
    assert_eq!(selections(&bridge.take_events()), [HostEvent::SelectionCleared], "the host's toolbar must not stay on stale text");
    h.run_steps(3);
    assert!(selections(&bridge.take_events()).is_empty(), "cleared once");
    // Clearing the blank selection, or selecting blank again, says nothing more.
    h.key_press(Key::Escape);
    h.run_steps(2);
    h.state_mut().views[0].select_text(0, 3, 3);
    h.run_steps(2);
    assert!(selections(&bridge.take_events()).is_empty(), "nothing was reported since the clear");
}

/// Open three pages and select "Definitions" on the first, so the host has been told of a selection.
fn with_reported_selection() -> (Harness<'static, PdfCraftApp>, HostBridge) {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    h.state_mut().views[0].select_text(0, 0, 10);
    h.run_steps(2);
    let events = selections(&bridge.take_events());
    assert!(matches!(events.as_slice(), [HostEvent::Selection { text, .. }] if text == "Definitions"), "{events:?}");
    (h, bridge)
}

/// Send a jump and return the selection events it caused, after a few more frames so a repeat would show.
fn selections_after_jump(h: &mut Harness<'static, PdfCraftApp>, bridge: &HostBridge, target: JumpTarget) -> Vec<HostEvent> {
    bridge.send(HostCommand::JumpTo(target));
    settle(h);
    h.run_steps(3);
    selections(&bridge.take_events())
}

fn text_target(text: &str, page: Option<u32>) -> JumpTarget {
    JumpTarget::Text { text: text.into(), page, occurrence: 0 }
}

#[test]
fn a_found_text_jump_clears_a_reported_selection_once() {
    let (mut h, bridge) = with_reported_selection();
    assert_eq!(selections_after_jump(&mut h, &bridge, text_target("thirty", None)), [HostEvent::SelectionCleared]);
    assert!(h.state().views[0].selected_text().is_none(), "the view's selection goes with it");
}

#[test]
fn a_found_page_jump_clears_a_reported_selection_once() {
    let (mut h, bridge) = with_reported_selection();
    assert_eq!(selections_after_jump(&mut h, &bridge, JumpTarget::Page { page: 2 }), [HostEvent::SelectionCleared]);
}

#[test]
fn a_find_step_clears_a_reported_selection_once() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    h.state_mut().views[0].open_find();
    if let Some(find) = h.state_mut().views[0].find.as_mut() {
        find.query = "goods".into();
    }
    // The search finishes over every page before the selection is made: the first match is the
    // find bar's own move, not a step.
    for _ in 0..500 {
        h.run_steps(2);
        if h.state().views[0].find.as_ref().is_some_and(|f| f.matches.len() == 3) && !h.state().render_pending() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.state_mut().views[0].select_text(0, 0, 10);
    h.run_steps(2);
    let events = selections(&bridge.take_events());
    assert!(matches!(events.as_slice(), [HostEvent::Selection { .. }]), "{events:?}");
    h.get_by_label("Next (⌘G)").click();
    h.run_steps(5);
    assert_eq!(selections(&bridge.take_events()), [HostEvent::SelectionCleared]);
}

#[test]
fn a_page_button_clears_a_reported_selection_once() {
    let (mut h, bridge) = with_reported_selection();
    h.get_by_label("Next page").click();
    h.run_steps(5);
    assert_eq!(selections(&bridge.take_events()), [HostEvent::SelectionCleared]);
    assert_eq!(h.state().views[0].current, 1);
}

#[test]
fn a_jump_that_finds_nothing_leaves_a_reported_selection_alone() {
    let (mut h, bridge) = with_reported_selection();
    for target in [text_target("no such passage", None), text_target("goods", Some(9)), JumpTarget::Page { page: 9 }] {
        assert!(selections_after_jump(&mut h, &bridge, target).is_empty());
    }
    assert!(h.state().views[0].selected_text().is_some(), "the selection stays");
    // It was never cleared, so a later move still clears it.
    assert_eq!(selections_after_jump(&mut h, &bridge, JumpTarget::Page { page: 2 }), [HostEvent::SelectionCleared]);
}

#[test]
fn opening_a_document_clears_a_reported_selection_before_opened() {
    let (mut h, bridge) = with_reported_selection();
    let events = open(&mut h, &bridge, "other.pdf", three_pages());
    assert_eq!(events, [HostEvent::SelectionCleared, HostEvent::Opened { pages: 3 }, HostEvent::PageChanged { page: 1 }]);
}

#[test]
fn without_a_reported_selection_jumps_find_steps_page_buttons_and_opens_send_no_clear() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    for target in [text_target("thirty", None), JumpTarget::Page { page: 1 }, text_target("missing", None)] {
        assert!(selections_after_jump(&mut h, &bridge, target).is_empty());
    }
    // The match is outlined, as the editor's own mark, and still not reported as a selection.
    assert!(selections_after_jump(&mut h, &bridge, text_target("goods", None)).is_empty());
    assert!(h.state().views[0].flash.is_some());
    h.get_by_label("Next page").click();
    h.run_steps(5);
    assert!(selections(&bridge.take_events()).is_empty());
    h.state_mut().views[0].open_find();
    h.run_steps(2);
    h.get_by_label("Next (⌘G)").click();
    h.run_steps(5);
    assert!(selections(&bridge.take_events()).is_empty());
    let events = open(&mut h, &bridge, "other.pdf", three_pages());
    assert_eq!(events, [HostEvent::Opened { pages: 3 }, HostEvent::PageChanged { page: 1 }]);
}

#[test]
fn a_selection_cleared_after_a_move_is_not_cleared_again_by_the_next_one() {
    let (mut h, bridge) = with_reported_selection();
    assert_eq!(selections_after_jump(&mut h, &bridge, JumpTarget::Page { page: 2 }), [HostEvent::SelectionCleared]);
    assert!(selections_after_jump(&mut h, &bridge, JumpTarget::Page { page: 3 }).is_empty());
    let events = open(&mut h, &bridge, "other.pdf", three_pages());
    assert_eq!(events, [HostEvent::Opened { pages: 3 }, HostEvent::PageChanged { page: 1 }]);
}
