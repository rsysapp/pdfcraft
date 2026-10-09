//! Host jumps and page events in the embed build.

mod embed_support;

use egui_kittest::Harness;
use embed_support::{embedded, open, settle, three_pages};
use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::embed::{HostBridge, HostCommand, HostEvent, JumpTarget};

fn text(text: &str, page: Option<u32>, occurrence: u32) -> JumpTarget {
    JumpTarget::Text { text: text.into(), page, occurrence }
}

fn not_found(events: &[HostEvent]) -> Vec<JumpTarget> {
    events
        .iter()
        .filter_map(|e| match e {
            HostEvent::JumpNotFound { target } => Some(target.clone()),
            _ => None,
        })
        .collect()
}

fn jump(h: &mut Harness<'static, PdfCraftApp>, bridge: &HostBridge, target: JumpTarget) -> Vec<HostEvent> {
    bridge.send(HostCommand::JumpTo(target));
    settle(h);
    bridge.take_events()
}

#[test]
fn opening_reports_the_first_page_and_page_jumps_report_the_new_one() {
    let (mut h, bridge) = embedded();
    let events = open(&mut h, &bridge, "three.pdf", three_pages());
    assert_eq!(events, [HostEvent::Opened { pages: 3 }, HostEvent::PageChanged { page: 1 }]);
    let events = jump(&mut h, &bridge, JumpTarget::Page { page: 3 });
    assert_eq!(h.state().views[0].current, 2);
    assert!(events.contains(&HostEvent::PageChanged { page: 3 }), "{events:?}");
    // u32::MAX is what a huge host integer saturates to.
    for page in [0, 4, u32::MAX] {
        assert_eq!(not_found(&jump(&mut h, &bridge, JumpTarget::Page { page })), [JumpTarget::Page { page }]);
    }
    assert_eq!(h.state().views[0].current, 2, "a refused jump stays put");
}

#[test]
fn text_jumps_fold_text_count_occurrences_and_stay_within_a_page() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    // The third "goods" in the document is on page 3 (case and page 1's quotes fold away).
    assert!(not_found(&jump(&mut h, &bridge, text("GOODS", None, 2))).is_empty());
    assert_eq!(h.state().views[0].current, 2);
    assert!(not_found(&jump(&mut h, &bridge, text("goods", Some(2), 0))).is_empty());
    assert_eq!(h.state().views[0].current, 1);
    for missing in [
        text("goods", Some(2), 1),
        text("within five days. payment", None, 0),
        text("goods", None, 3),
        text("goods", None, u32::MAX),
        text("goods", Some(7), 0),
        text("goods", Some(u32::MAX), 0),
        text("   ", None, 0),
    ] {
        assert_eq!(not_found(&jump(&mut h, &bridge, missing.clone())), [missing]);
    }
    assert_eq!(h.state().views[0].current, 1);
}

#[test]
fn text_jumps_match_curly_quotes_and_span_lines() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    for target in [text("\"goods\" are the packaging", Some(1), 0), text("packaging   DESCRIBED in", None, 0)] {
        h.state_mut().views[0].flash = None;
        assert!(not_found(&jump(&mut h, &bridge, target)).is_empty());
        assert_eq!(h.state().views[0].flash.map(|f| f.0), Some(0), "the passage on page 1 is outlined");
    }
}

#[test]
fn jumps_without_a_document_or_overtaken_by_an_open() {
    let (mut h, bridge) = embedded();
    h.run_steps(2);
    for target in [JumpTarget::Page { page: 1 }, text("anything", None, 0)] {
        assert_eq!(not_found(&jump(&mut h, &bridge, target.clone())), [target]);
    }
    open(&mut h, &bridge, "three.pdf", three_pages());
    // Page 3's text layer isn't loaded yet, so this jump waits; the open replaces the document first.
    bridge.send(HostCommand::JumpTo(text("thirty", None, 0)));
    bridge.send(HostCommand::Open { name: "other.pdf".into(), bytes: three_pages() });
    h.run_steps(3);
    settle(&mut h);
    let events = bridge.take_events();
    assert!(not_found(&events).is_empty(), "{events:?}");
    assert_eq!(events.first(), Some(&HostEvent::Opened { pages: 3 }));
}

#[test]
fn a_newer_jump_replaces_one_still_waiting_for_text() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "three.pdf", three_pages());
    // "thirty" is on page 3, whose text layer isn't loaded yet, so the text jump waits; the page jump
    // arrives before it can finish and replaces it.
    bridge.send(HostCommand::JumpTo(text("thirty", None, 0)));
    bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 2 }));
    h.run_steps(1);
    assert!(!h.state().host_work_pending(), "the page jump cleared the waiting text jump");
    settle(&mut h);
    // Give a late scroll for the first jump every chance to show up.
    h.run_steps(10);
    let events = bridge.take_events();
    assert!(not_found(&events).is_empty(), "{events:?}");
    assert_eq!(h.state().views[0].current, 1, "only the page jump took effect");
    assert!(h.state().views[0].flash.is_none(), "the replaced text jump outlined nothing");
}
