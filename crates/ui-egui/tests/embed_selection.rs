//! Selection events in the embed build: sent when a selection settles.

mod embed_support;

use egui::{Event, Key, Modifiers, PointerButton};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use embed_support::{embedded, open, pages, settle, three_pages};
use pdfcraft_ui_egui::embed::{HostBridge, HostCommand, HostEvent, JumpTarget};
use pdfcraft_ui_egui::{PdfCraftApp, QuickTool};

fn selections(events: &[HostEvent]) -> Vec<HostEvent> {
    events.iter().filter(|e| matches!(e, HostEvent::Selection { .. } | HostEvent::SelectionCleared)).cloned().collect()
}

fn two_lines() -> Vec<u8> {
    pages(&[&["The quick brown fox", "jumps over the dog"]])
}

/// Where glyphs 4 ("q") and 8 ("k") of [`two_lines`]'s first line are: "The" 0–2, a space 3, "quick" 4–8.
fn quick_ends(h: &Harness<'static, PdfCraftApp>) -> (egui::Pos2, egui::Pos2) {
    let view = &h.state().views[0];
    (view.glyph_screen_pos(0, 4).expect("q on screen"), view.glyph_screen_pos(0, 8).expect("k on screen"))
}

/// Press at `a`, drag to `b` and hold the button there; `release` lets it go.
fn drag_and_hold(h: &mut Harness<'static, PdfCraftApp>, a: egui::Pos2, b: egui::Pos2) {
    h.hover_at(a);
    h.run_steps(1);
    h.event(Event::PointerButton { pos: a, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.run_steps(1);
    for k in 1..=4 {
        h.event(Event::PointerMoved(a + (b - a) * (k as f32 / 4.0)));
        h.run_steps(1);
    }
}

fn release(h: &mut Harness<'static, PdfCraftApp>, at: egui::Pos2) {
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
}

/// Drag from `a` to `b` by hand and let go.
fn drag(h: &mut Harness<'static, PdfCraftApp>, a: egui::Pos2, b: egui::Pos2) {
    drag_and_hold(h, a, b);
    release(h, b);
}

/// A click (press and release without moving) at `at`.
fn click_at(h: &mut Harness<'static, PdfCraftApp>, at: egui::Pos2) {
    h.hover_at(at);
    h.run_steps(1);
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.run_steps(1);
    release(h, at);
}

/// A point on the pasteboard beside the first page: no text, no control.
fn pasteboard(h: &Harness<'static, PdfCraftApp>) -> egui::Pos2 {
    let view = &h.state().views[0];
    let page = view.page_screen_rect(0).expect("on screen");
    let at = egui::pos2(page.max.x + 30.0, page.center().y);
    assert!(view.viewport_rect().shrink(20.0).contains(at), "{at:?} lies in the document area {:?}", view.viewport_rect());
    at
}

fn is_selection_of(events: &[HostEvent], want: &str) -> bool {
    matches!(events, [HostEvent::Selection { text, .. }] if text == want)
}

#[test]
fn a_dragged_selection_is_reported_once_the_button_is_released() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "lines.pdf", two_lines());
    let (a, b) = quick_ends(&h);
    drag_and_hold(&mut h, a, b);
    assert!(selections(&bridge.take_events()).is_empty(), "nothing while the button is down");
    release(&mut h, b);
    let events = selections(&bridge.take_events());
    let [HostEvent::Selection { text, copy_text, page, x, y }] = events.as_slice() else { panic!("one selection event: {events:?}") };
    assert_eq!((text.as_str(), copy_text.as_str(), *page), ("quick", "quick", Some(1)));
    let page_rect = h.state().views[0].page_screen_rect(0).expect("on screen");
    assert!(*x >= b.x && *y >= b.y && page_rect.expand(1.0).contains(egui::pos2(*x, *y)), "({x}, {y}) ends the selection");
    h.run_steps(3);
    assert!(selections(&bridge.take_events()).is_empty(), "reported once");
}

#[test]
fn selecting_the_same_passage_again_is_reported_again() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "lines.pdf", two_lines());
    let (a, b) = quick_ends(&h);
    drag(&mut h, a, b);
    assert!(is_selection_of(&selections(&bridge.take_events()), "quick"));
    // The host hid its toolbar on the press; the same passage selected again must bring it back.
    drag(&mut h, a, b);
    let events = selections(&bridge.take_events());
    assert!(is_selection_of(&events, "quick"), "a second selection event for the same passage: {events:?}");
    // Settled, it is not told again frame after frame.
    h.run_steps(5);
    assert!(selections(&bridge.take_events()).is_empty(), "once per settling");
    // A press and release that leave it as it is (here, on the pasteboard) settle it again too.
    let beside = pasteboard(&h);
    click_at(&mut h, beside);
    let events = selections(&bridge.take_events());
    assert!(is_selection_of(&events, "quick"), "told again on release: {events:?}");
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("quick"));
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
    // The view's selection is now the match, the editor's own, and is never reported.
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("thirty"), "the reported selection made way for the match");
}

#[test]
fn a_found_text_jump_selects_its_match_and_never_reports_it() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "lines.pdf", two_lines());
    // The match is selected, so it is highlighted as a selection is, and stays unreported.
    assert!(selections_after_jump(&mut h, &bridge, text_target("QUICK", None)).is_empty());
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("quick"));
    // A press and release that leave it as it is do not make it a person's selection.
    let beside = pasteboard(&h);
    click_at(&mut h, beside);
    h.run_steps(3);
    assert!(selections(&bridge.take_events()).is_empty(), "the editor's own selection is never reported");
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("quick"));
    // Selecting the same passage by hand is a person's selection: it is reported.
    let (a, b) = quick_ends(&h);
    drag(&mut h, a, b);
    let events = selections(&bridge.take_events());
    assert!(is_selection_of(&events, "quick"), "the person's selection of the jump's match: {events:?}");
}

#[test]
fn a_found_text_jump_replaces_a_jumps_selection_and_a_miss_keeps_it() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    assert!(selections_after_jump(&mut h, &bridge, text_target("five days", None)).is_empty());
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("five days"));
    assert!(selections_after_jump(&mut h, &bridge, text_target("no such passage", None)).is_empty());
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("five days"), "a jump that finds nothing leaves it");
    assert!(selections_after_jump(&mut h, &bridge, text_target("thirty", Some(3))).is_empty());
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("thirty"));
}

#[test]
fn moving_the_view_clears_a_jumps_selection_without_a_word() {
    // A page jump, a page button, a find step and an open each move the view.
    let jumped = || {
        let (mut h, bridge) = embedded();
        open(&mut h, &bridge, "three.pdf", three_pages());
        assert!(selections_after_jump(&mut h, &bridge, text_target("thirty", None)).is_empty());
        assert_eq!(h.state().views[0].selected_text().as_deref(), Some("thirty"));
        (h, bridge)
    };
    let (mut h, bridge) = jumped();
    assert!(selections_after_jump(&mut h, &bridge, JumpTarget::Page { page: 1 }).is_empty());
    assert!(h.state().views[0].selected_text().is_none(), "a page jump clears the match");

    let (mut h, bridge) = jumped();
    h.get_by_label("Previous page").click();
    h.run_steps(5);
    assert!(selections(&bridge.take_events()).is_empty());
    assert!(h.state().views[0].selected_text().is_none(), "a page button clears the match");

    let (mut h, bridge) = jumped();
    search_goods(&mut h);
    assert!(selections_after_jump(&mut h, &bridge, text_target("thirty", None)).is_empty());
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("thirty"));
    h.get_by_label("Next (⌘G)").click();
    h.run_steps(5);
    assert!(selections(&bridge.take_events()).is_empty());
    assert!(h.state().views[0].selected_text().is_none(), "a find step clears the match");

    let (mut h, bridge) = jumped();
    let events = open(&mut h, &bridge, "other.pdf", three_pages());
    assert_eq!(events, [HostEvent::Opened { pages: 3 }, HostEvent::PageChanged { page: 1 }]);
    assert!(h.state().views[0].selected_text().is_none());
}

#[test]
fn a_found_page_jump_clears_a_reported_selection_once() {
    let (mut h, bridge) = with_reported_selection();
    assert_eq!(selections_after_jump(&mut h, &bridge, JumpTarget::Page { page: 2 }), [HostEvent::SelectionCleared]);
}

/// Search for "goods" in the find bar and wait until it found every match (three pages): the first
/// match is the find bar's own move, not a step.
fn search_goods(h: &mut Harness<'static, PdfCraftApp>) {
    h.state_mut().views[0].open_find();
    if let Some(find) = h.state_mut().views[0].find.as_mut() {
        find.query = "goods".into();
    }
    for _ in 0..500 {
        h.run_steps(2);
        if h.state().views[0].find.as_ref().is_some_and(|f| f.matches.len() == 3) && !h.state().render_pending() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the find bar did not find the three matches");
}

#[test]
fn a_find_step_clears_a_reported_selection_once() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    // The search finishes over every page before the selection is made.
    search_goods(&mut h);
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

/// A wheel scroll over the page view by `dy` points (positive moves the content down), run until
/// the scroll has finished animating.
fn wheel(h: &mut Harness<'static, PdfCraftApp>, dy: f32) {
    let at = h.state().views[0].viewport_rect().center();
    h.hover_at(at);
    h.run_steps(1);
    h.event(Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, dy),
        phase: egui::TouchPhase::Move,
        modifiers: Modifiers::NONE,
    });
    h.run_steps(30);
}

/// A point on the pasteboard beside whichever page is current: no text, no control.
fn pasteboard_now(h: &Harness<'static, PdfCraftApp>) -> egui::Pos2 {
    let view = &h.state().views[0];
    let page = view.page_screen_rect(view.current).expect("the current page is on screen");
    let at = egui::pos2(page.max.x + 30.0, view.viewport_rect().center().y);
    assert!(view.viewport_rect().shrink(20.0).contains(at), "{at:?} lies in the document area {:?}", view.viewport_rect());
    at
}

#[test]
fn a_press_on_the_controls_does_not_report_the_selection_again() {
    let (mut h, bridge) = with_reported_selection();
    // The zoom controls and the page display menu, in the navigation rail.
    for label in ["Zoom in (⌘+)", "Zoom out (⌘−)"] {
        h.get_by_label(label).click();
        h.run_steps(5);
        assert!(selections(&bridge.take_events()).is_empty(), "a press on {label} selects nothing");
    }
    // Opened and closed again by its button (Escape would clear the selection).
    for _ in 0..2 {
        h.get_by_label_contains("Page display: ").click();
        h.run_steps(3);
    }
    assert!(selections(&bridge.take_events()).is_empty(), "a press on the page display menu selects nothing");
    // The scrollbar, at the page view's right edge (three pages do not fit).
    let viewport = h.state().views[0].viewport_rect();
    let bar = egui::pos2(viewport.right() - 3.0, viewport.center().y);
    click_at(&mut h, bar);
    h.run_steps(3);
    assert!(selections(&bridge.take_events()).is_empty(), "a press on the scrollbar selects nothing");
    // The find field, floating over the page view.
    h.state_mut().views[0].open_find();
    h.run_steps(3);
    let field = h
        .query_all_by_role(egui::accesskit::Role::TextInput)
        .map(|node| node.rect())
        .find(|rect| viewport.contains(rect.center()))
        .expect("the find field lies over the page view");
    click_at(&mut h, field.center());
    h.run_steps(3);
    assert!(selections(&bridge.take_events()).is_empty(), "a press in the find field selects nothing");
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("Definitions"), "the selection stays");
    // A press on the page view after all that still is input on the selection (the scrollbar
    // press scrolled, so beside whichever page is shown).
    let beside = pasteboard_now(&h);
    click_at(&mut h, beside);
    let events = selections(&bridge.take_events());
    assert!(is_selection_of(&events, "Definitions"), "told again on a press on the page view: {events:?}");
}

#[test]
fn a_press_on_the_page_reports_the_selection_again() {
    let (mut h, bridge) = with_reported_selection();
    // With the Hand tool, so the press pans instead of selecting: the selection stays as it is.
    h.state_mut().quick_tool = QuickTool::Hand;
    h.run_steps(2);
    let page = h.state().views[0].page_screen_rect(0).expect("on screen");
    click_at(&mut h, page.center());
    let events = selections(&bridge.take_events());
    assert!(is_selection_of(&events, "Definitions"), "told again on release: {events:?}");
    h.run_steps(5);
    assert!(selections(&bridge.take_events()).is_empty(), "once per press");
}

#[test]
fn a_selection_off_screen_is_not_reported_again_when_a_wheel_scroll_brings_it_back() {
    let (mut h, bridge) = with_reported_selection();
    // Scroll the selected page off screen: no press, nothing to tell.
    wheel(&mut h, -2000.0);
    assert!(h.state().views[0].page_screen_rect(0).is_none(), "page 1 is off screen");
    assert!(selections(&bridge.take_events()).is_empty());
    // A press on the page view asks for the selection again, but it is off screen: not ready.
    let beside = pasteboard_now(&h);
    click_at(&mut h, beside);
    h.run_steps(3);
    assert!(selections(&bridge.take_events()).is_empty(), "nothing while the selection is off screen");
    // Scrolling it back into view is no input on it, so nothing is told.
    wheel(&mut h, 2000.0);
    assert!(h.state().views[0].page_screen_rect(0).is_some(), "page 1 is back");
    h.run_steps(5);
    assert!(selections(&bridge.take_events()).is_empty(), "a wheel scroll reports nothing");
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("Definitions"));
}

#[test]
fn a_jump_that_lands_while_the_button_is_held_is_not_reported() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "lines.pdf", two_lines());
    // Glyphs of the first line: "The" 0–2, a space 3, "quick" 4–8, a space 9, "brown" 10–14.
    let at = |h: &Harness<'static, PdfCraftApp>, glyph: usize| h.state().views[0].glyph_screen_pos(0, glyph).expect("on screen");
    let (q, k) = (at(&h, 4), at(&h, 8));
    drag_and_hold(&mut h, q, k);
    // The jump lands under the held button and selects its match, the editor's own selection.
    bridge.send(HostCommand::JumpTo(text_target("brown", None)));
    settle(&mut h);
    // The held drag moves its end on the match's line, then ends on the match's own end.
    let n = at(&h, 14);
    for k in 1..=4 {
        h.event(Event::PointerMoved(k_to(at(&h, 6), n, k)));
        h.run_steps(1);
    }
    release(&mut h, n);
    h.run_steps(3);
    let events = selections(&bridge.take_events());
    assert!(events.is_empty(), "the jump's own selection is never reported: {events:?}");
    assert_eq!(h.state().views[0].selected_text().as_deref(), Some("brown"));
}

/// The `k`th of four steps from `a` to `b`.
fn k_to(a: egui::Pos2, b: egui::Pos2, k: usize) -> egui::Pos2 {
    a + (b - a) * (k as f32 / 4.0)
}

#[test]
fn copy_after_a_found_text_jump_copies_the_jumped_text() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    assert!(selections_after_jump(&mut h, &bridge, text_target("THIRTY days", None)).is_empty());
    h.event(Event::Copy);
    let mut copied = false;
    for _ in 0..4 {
        h.step();
        copied |= h.output().platform_output.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(t) if t == "thirty days"));
    }
    assert!(copied, "⌘C copies the match the jump selected, as the document spells it");
    assert!(selections(&bridge.take_events()).is_empty(), "copying reports nothing");
}
