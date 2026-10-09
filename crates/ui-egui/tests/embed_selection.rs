//! Selection events in the embed build: sent when a selection settles.

mod embed_support;

use egui::{Event, Key, Modifiers, PointerButton};
use embed_support::{embedded, open, pages};
use pdfcraft_ui_egui::embed::HostEvent;

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
