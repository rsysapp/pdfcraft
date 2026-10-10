//! `setTheme`: light or dark, and the host page's accent.

mod embed_support;

use egui::{Color32, ViewportCommand, ViewportId};
use egui_kittest::Harness;
use embed_support::{embedded, embedded_with};
use pdfcraft_ui_egui::PdfCraftApp;
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

/// Send the host's theme, then count the `SetTheme` viewport commands the frames that follow sent.
fn window_theme_commands(h: &mut Harness<'static, PdfCraftApp>, bridge: &pdfcraft_ui_egui::embed::HostBridge, dark: bool) -> usize {
    bridge.send(HostCommand::SetTheme(Theme { dark, accent: Color32::from_rgb(0x0F, 0x76, 0x6E), accent_content: Color32::WHITE }));
    let mut sent = 0;
    for _ in 0..4 {
        h.step();
        let commands = &h.output().viewport_output[&ViewportId::ROOT].commands;
        sent += commands.iter().filter(|command| matches!(command, ViewportCommand::SetTheme(_))).count();
    }
    sent
}

#[test]
fn a_host_theme_change_sends_no_window_theme_command() {
    let (mut h, bridge) = embedded();
    h.run_steps(2);
    assert_eq!(window_theme_commands(&mut h, &bridge, true), 0, "dark");
    assert_eq!(window_theme_commands(&mut h, &bridge, false), 0, "dark to light");
    assert!(!Tokens::get(&h.ctx).dark(), "the theme itself still changes");
    assert_eq!(h.ctx.options(|o| o.theme_preference), egui::ThemePreference::Light, "the preference stays explicit");
}

#[test]
fn without_the_embed_context_setup_a_theme_change_sends_one() {
    // What the test above rules out: egui's window-theme sync, which the web backend only logs.
    let (mut h, bridge) = embedded_with(false);
    h.run_steps(2);
    window_theme_commands(&mut h, &bridge, true);
    assert!(window_theme_commands(&mut h, &bridge, false) > 0);
}
