//! `setTheme`: light or dark, and the host page's accent.

mod embed_support;

use egui::Color32;
use embed_support::embedded;
use pdfcraft_ui_egui::embed::{HostCommand, Theme};
use pdfcraft_ui_egui::theme::{ThemeKind, Tokens};

#[test]
fn set_theme_switches_to_dark_and_uses_the_host_accent() {
    let (mut h, bridge) = embedded();
    let teal = Color32::from_rgb(0x0F, 0x76, 0x6E);
    bridge.send(HostCommand::SetTheme(Theme { dark: true, accent: teal, accent_content: Color32::WHITE }));
    h.run_steps(3);
    let tokens = Tokens::get(&h.ctx);
    assert!(tokens.dark());
    assert_eq!(tokens.accent, teal);
    assert_eq!(h.ctx.global_style().visuals.selection.stroke.color, Color32::WHITE, "text on the accent");
    assert_eq!(h.state().theme, ThemeKind::Dark);
    h.run_steps(5);
    assert_eq!(Tokens::get(&h.ctx).accent, teal, "the app's own theme sync keeps it");
    let red = Color32::from_rgb(0xB4, 0x23, 0x18);
    bridge.send(HostCommand::SetTheme(Theme { dark: false, accent: red, accent_content: Color32::WHITE }));
    h.run_steps(2);
    assert!(!Tokens::get(&h.ctx).dark());
    assert_eq!(Tokens::get(&h.ctx).accent, red);
}

#[test]
fn without_a_host_theme_the_default_tokens_are_untouched() {
    let (mut h, _bridge) = embedded();
    h.run_steps(3);
    let defaults = Tokens::for_kind(ThemeKind::Light);
    let tokens = Tokens::get(&h.ctx);
    assert!(!tokens.dark());
    assert_eq!(tokens.accent, defaults.accent);
    assert_eq!(tokens.accent_text, defaults.accent_text);
    assert_eq!(tokens.accent_soft, defaults.accent_soft);
    assert_eq!(tokens.badge_new, defaults.badge_new);
}
