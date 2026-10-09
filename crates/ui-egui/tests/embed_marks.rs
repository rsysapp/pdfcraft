//! Host marks in the embed build: found by text, painted, and clickable.

mod embed_support;

use egui::{Color32, Event, Modifiers, PointerButton};
use embed_support::{embedded, open, pages, settle, three_pages};
use pdfcraft_ui_egui::embed::{HostCommand, HostEvent, Mark, parse_color};

fn mark(id: &str, text: &str, page: Option<u32>, color: &str) -> Mark {
    Mark { id: id.into(), text: text.into(), page, color: parse_color(color).expect("colour") }
}

fn press(h: &mut egui_kittest::Harness<'static, pdfcraft_ui_egui::PdfCraftApp>, at: egui::Pos2, pressed: bool) {
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
}

#[test]
fn marks_are_found_where_they_may_lie_and_skipped_otherwise() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    bridge.send(HostCommand::SetMarks(vec![
        mark("goods", "THE   goods", None, "#ff000080"),
        mark("thirty", "within thirty days", Some(3), "#00ff0080"),
        mark("elsewhere", "within thirty days", Some(2), "#0000ff80"),
        mark("missing", "not in this file", None, "#00000080"),
    ]));
    settle(&mut h);
    let red = Color32::from_rgba_unmultiplied(255, 0, 0, 128);
    let green = Color32::from_rgba_unmultiplied(0, 255, 0, 128);
    assert_eq!(h.state().found_host_marks(), [("goods".to_string(), 2, red), ("thirty".to_string(), 3, green)]);
    assert!(bridge.take_events().iter().all(|e| !matches!(e, HostEvent::JumpNotFound { .. })), "marks that aren't found stay silent");
    bridge.send(HostCommand::SetMarks(Vec::new()));
    settle(&mut h);
    assert!(h.state().found_host_marks().is_empty(), "an empty array clears them");
}

#[test]
fn clicking_a_mark_tells_the_host_and_opening_clears_marks() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "fox.pdf", pages(&[&["The quick brown fox"]]));
    bridge.send(HostCommand::SetMarks(vec![mark("q", "quick brown", Some(1), "#ffd60080")]));
    settle(&mut h);
    h.run_steps(1);
    let rects = h.state().host_mark_rects();
    let [(id, rect)] = rects.as_slice() else { panic!("one mark on screen: {rects:?}") };
    assert_eq!(id, "q");
    let at = rect.center();
    h.hover_at(at);
    h.run_steps(1);
    bridge.take_events();
    for pressed in [true, false] {
        press(&mut h, at, pressed);
    }
    h.run_steps(2);
    let activations: Vec<HostEvent> = bridge.take_events().into_iter().filter(|e| matches!(e, HostEvent::MarkActivated { .. })).collect();
    assert_eq!(activations, [HostEvent::MarkActivated { id: "q".into() }], "one click, one event");
    open(&mut h, &bridge, "fox.pdf", pages(&[&["The quick brown fox"]]));
    assert!(h.state().found_host_marks().is_empty(), "a new document starts without marks");
}

#[test]
fn overlapping_marks_activate_the_one_painted_last() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "fox.pdf", pages(&[&["The quick brown fox"]]));
    bridge.send(HostCommand::SetMarks(vec![mark("outer", "quick brown fox", Some(1), "#ffd60080"), mark("inner", "brown", Some(1), "#00ff0080")]));
    settle(&mut h);
    h.run_steps(1);
    let rects = h.state().host_mark_rects();
    let inner = rects.iter().find(|(id, _)| id == "inner").expect("the inner mark is on screen").1;
    let at = inner.center();
    h.hover_at(at);
    h.run_steps(1);
    bridge.take_events();
    for pressed in [true, false] {
        press(&mut h, at, pressed);
    }
    h.run_steps(2);
    let activations: Vec<HostEvent> = bridge.take_events().into_iter().filter(|e| matches!(e, HostEvent::MarkActivated { .. })).collect();
    assert_eq!(activations, [HostEvent::MarkActivated { id: "inner".into() }], "the later mark is on top");
}

#[test]
fn dragging_a_selection_across_a_mark_does_not_activate_it() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "fox.pdf", pages(&[&["The quick brown fox"]]));
    bridge.send(HostCommand::SetMarks(vec![mark("q", "quick brown", Some(1), "#ffd60080")]));
    settle(&mut h);
    h.run_steps(1);
    let rects = h.state().host_mark_rects();
    let [(_, rect)] = rects.as_slice() else { panic!("one mark on screen: {rects:?}") };
    let (from, to) = (rect.left_center() + egui::vec2(2.0, 0.0), rect.right_center() - egui::vec2(2.0, 0.0));
    h.hover_at(from);
    h.run_steps(1);
    bridge.take_events();
    press(&mut h, from, true);
    h.run_steps(1);
    for step in 1..=6 {
        h.hover_at(from.lerp(to, step as f32 / 6.0));
        h.run_steps(1);
    }
    press(&mut h, to, false);
    h.run_steps(3);
    let events = bridge.take_events();
    assert!(events.iter().any(|e| matches!(e, HostEvent::Selection { .. })), "the drag selected text: {events:?}");
    assert!(events.iter().all(|e| !matches!(e, HostEvent::MarkActivated { .. })), "a drag is not a click: {events:?}");
}

#[test]
fn page_scoped_marks_load_their_own_pages_and_out_of_range_ones_are_skipped() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    bridge.send(HostCommand::SetMarks(vec![
        mark("late", "within thirty days", Some(3), "#00ff0080"),
        mark("early", "Definitions", Some(1), "#0000ff80"),
        mark("beyond", "Delivery", Some(9), "#ff00ff80"),
        mark("zero", "Delivery", Some(0), "#ff00ff80"),
        mark("saturated", "Delivery", Some(u32::MAX), "#ff00ff80"),
    ]));
    settle(&mut h);
    let found: Vec<(String, usize)> = h.state().found_host_marks().into_iter().map(|(id, page, _)| (id, page)).collect();
    assert_eq!(found, [("early".to_string(), 1), ("late".to_string(), 3)]);
}

#[test]
fn marks_that_can_never_match_load_no_text() {
    let (mut h, bridge) = embedded();
    let filler: &[&str] = &["Clause", "Plain filler text"];
    open(&mut h, &bridge, "thirty.pdf", pages(&[filler; 30]));
    assert!(!h.state().views[0].has_text(12), "a page far from the view starts without its text");
    let oversized = "x".repeat(pdfcraft_ui_egui::embed::text_match::MAX_NEEDLE_CHARS + 1);
    bridge.send(HostCommand::SetMarks(vec![
        mark("empty", "", None, "#ff000080"),
        mark("blank", " \t ", None, "#ff000080"),
        mark("long", &oversized, None, "#ff000080"),
    ]));
    settle(&mut h);
    assert!(h.state().found_host_marks().is_empty());
    assert!(!h.state().views[0].has_text(12), "skipped marks ask for no page's text");
}
