//! The host page's colours: light or dark, and its accent in place of PdfCraft's blue.

use egui::{Color32, Stroke};

use super::Theme;
use crate::theme::{ThemeKind, Tokens};

/// Apply `theme`: the light or dark tokens and egui style first, then the host's accent.
pub(crate) fn apply(ctx: &egui::Context, theme: &Theme) {
    let kind = if theme.dark { ThemeKind::Dark } else { ThemeKind::Light };
    crate::theme::apply(ctx, kind);
    let mut tokens = Tokens::for_kind(kind);
    tokens.accent = theme.accent;
    tokens.accent_text = theme.accent;
    tokens.badge_new = theme.accent;
    tokens.accent_soft = mix(theme.accent, tokens.panel, 0.18);
    // The key `Tokens::get` reads (a test checks the override is seen).
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("pdfcraft-theme"), tokens));
    ctx.global_style_mut(|style| {
        style.visuals.selection.bg_fill = theme.accent;
        style.visuals.selection.stroke = Stroke::new(1.0, theme.accent_content);
        style.visuals.hyperlink_color = theme.accent;
    });
}

/// `weight` of `over` on `base`; both opaque.
fn mix(over: Color32, base: Color32, weight: f32) -> Color32 {
    let channel = |a: u8, b: u8| (f32::from(a) * weight + f32::from(b) * (1.0 - weight)).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(channel(over.r(), base.r()), channel(over.g(), base.g()), channel(over.b(), base.b()))
}
