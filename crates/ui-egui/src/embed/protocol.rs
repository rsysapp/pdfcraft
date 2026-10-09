//! The host bridge's messages (the embed API's commands and events) and the queues that carry them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use egui::Color32;

/// Most marks one `setMarks` keeps; the rest are ignored.
pub const MAX_MARKS: usize = 2_000;

/// Where a host jump goes.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum JumpTarget {
    /// A page, 1-based.
    Page {
        #[serde(deserialize_with = "saturating_u32")]
        page: u32,
    },
    /// The `occurrence`-th (0-based) match of `text` on `page` (1-based), or in the document.
    Text {
        text: String,
        #[serde(default, deserialize_with = "saturating_optional_u32")]
        page: Option<u32>,
        #[serde(deserialize_with = "saturating_u32")]
        occurrence: u32,
    },
}

/// A non-negative JSON integer, saturated to `u32::MAX`: JavaScript integers reach 2^53, and one
/// beyond `u32` is merely out of range for any document (the jump answers not found), not malformed.
fn saturating_u32<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    Ok(u32::try_from(value).unwrap_or(u32::MAX))
}

fn saturating_optional_u32<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let value = <Option<u64> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.map(|number| u32::try_from(number).unwrap_or(u32::MAX)))
}

/// A passage the host wants painted, validated.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub id: String,
    pub text: String,
    /// 1-based; `None` searches every page.
    pub page: Option<u32>,
    pub color: Color32,
}

/// The host page's colours, validated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub accent: Color32,
    /// Text on the accent.
    pub accent_content: Color32,
}

/// What the host asks for. The embed crate builds these from JavaScript values.
#[derive(Clone, Debug, PartialEq)]
pub enum HostCommand {
    Open { name: String, bytes: Vec<u8> },
    JumpTo(JumpTarget),
    SetMarks(Vec<Mark>),
    SetTheme(Theme),
}

/// What the page view tells the host; serialised as the API's `EditorEvent`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HostEvent {
    Opened {
        pages: usize,
    },
    OpenFailed {
        message: String,
    },
    PageChanged {
        page: usize,
    },
    Selection {
        text: String,
        #[serde(rename = "copyText")]
        copy_text: String,
        page: Option<usize>,
        x: f32,
        y: f32,
    },
    SelectionCleared,
    MarkActivated {
        id: String,
    },
    JumpNotFound {
        target: JumpTarget,
    },
}

impl HostEvent {
    /// The event as JSON, the shape the host's listener receives.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }
}

/// The two queues between a host page and the running app, shared by both sides. A poisoned lock
/// still holds valid queues, so its contents are used rather than dropped.
#[derive(Clone, Default)]
pub struct HostBridge {
    commands: Arc<Mutex<VecDeque<HostCommand>>>,
    events: Arc<Mutex<Vec<HostEvent>>>,
    /// The app's context once it has run a frame, so a command wakes an idle app.
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl HostBridge {
    /// Host side: queue a command; the app runs it on its next frame, which this requests.
    pub fn send(&self, command: HostCommand) {
        self.commands.lock().unwrap_or_else(PoisonError::into_inner).push_back(command);
        // Clone out and release the lock first: `request_repaint` takes egui's own lock, and a host
        // thread must not hold ours while it waits for that.
        let ctx = self.ctx.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }

    /// Host side: every event since the last call, oldest first.
    pub fn take_events(&self) -> Vec<HostEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// App side: the context `send` wakes.
    pub fn attach(&self, ctx: &egui::Context) {
        let mut slot = self.ctx.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(ctx.clone());
        }
    }

    /// App side: every command since the last call, oldest first.
    pub fn take_commands(&self) -> Vec<HostCommand> {
        self.commands.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect()
    }

    /// App side: tell the host something.
    pub fn emit(&self, event: HostEvent) {
        self.events.lock().unwrap_or_else(PoisonError::into_inner).push(event);
    }
}

#[derive(serde::Deserialize)]
struct WireMark {
    id: String,
    text: String,
    page: Option<u32>,
    color: String,
}

#[derive(serde::Deserialize)]
struct WireTheme {
    dark: bool,
    accent: String,
    #[serde(rename = "accentContent")]
    accent_content: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireMode {
    View,
}

#[derive(serde::Deserialize)]
struct WireMount {
    mode: WireMode,
    theme: WireTheme,
}

/// `JumpTarget` from the host's JSON.
pub fn parse_jump_target(json: &str) -> Result<JumpTarget, String> {
    serde_json::from_str(json).map_err(|e| format!("jumpTo: {e}"))
}

/// `Mark[]` from the host's JSON: each colour `#rrggbbaa` (or `#rrggbb`). Marks beyond [`MAX_MARKS`]
/// are ignored, as the embed API says, so a long list still paints its first part.
pub fn parse_marks(json: &str) -> Result<Vec<Mark>, String> {
    let wire: Vec<WireMark> = serde_json::from_str(json).map_err(|e| format!("setMarks: {e}"))?;
    wire.into_iter()
        .take(MAX_MARKS)
        .map(|m| {
            let color = parse_color(&m.color).ok_or_else(|| format!("setMarks: mark {:?} has colour {:?}; expected #rrggbbaa", m.id, m.color))?;
            Ok(Mark { id: m.id, text: m.text, page: m.page, color })
        })
        .collect()
}

/// `Theme` from the host's JSON.
pub fn parse_theme(json: &str) -> Result<Theme, String> {
    let wire: WireTheme = serde_json::from_str(json).map_err(|e| format!("setTheme: {e}"))?;
    theme_from(wire).map_err(|e| format!("setTheme: {e}"))
}

/// `MountOptions` from the host's JSON: only `mode: 'view'` exists in this version. Returns the
/// starting theme.
pub fn parse_mount_options(json: &str) -> Result<Theme, String> {
    let wire: WireMount = serde_json::from_str(json).map_err(|e| format!("mount: {e}"))?;
    match wire.mode {
        WireMode::View => theme_from(wire.theme).map_err(|e| format!("mount: {e}")),
    }
}

fn theme_from(wire: WireTheme) -> Result<Theme, String> {
    // The theme's colours are opaque: exactly `#rrggbb`.
    let colour =
        |name: &str, value: &str| parse_color(value).filter(|_| value.len() == 7).ok_or_else(|| format!("{name} {value:?} is not a #rrggbb colour"));
    Ok(Theme { dark: wire.dark, accent: colour("accent", &wire.accent)?, accent_content: colour("accentContent", &wire.accent_content)? })
}

/// `#rrggbb` (opaque) or `#rrggbbaa`, either case.
pub fn parse_color(value: &str) -> Option<Color32> {
    let hex = value.strip_prefix('#')?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| hex.get(at..at + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
    let alpha = if hex.len() == 8 { byte(6)? } else { 255 };
    Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, alpha))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use egui::Color32;
    use serde_json::{Value, json};

    use super::*;

    fn shape(event: &HostEvent) -> Value {
        serde_json::from_str(&event.to_json().expect("serialises")).expect("JSON")
    }

    #[test]
    fn jump_targets_parse_as_the_api_writes_them() {
        assert_eq!(parse_jump_target(r#"{"kind":"page","page":3}"#), Ok(JumpTarget::Page { page: 3 }));
        assert_eq!(
            parse_jump_target(r#"{"kind":"text","text":"the Goods","page":null,"occurrence":1}"#),
            Ok(JumpTarget::Text { text: "the Goods".into(), page: None, occurrence: 1 })
        );
        assert_eq!(
            parse_jump_target(r#"{"kind":"text","text":"x","page":2,"occurrence":0}"#),
            Ok(JumpTarget::Text { text: "x".into(), page: Some(2), occurrence: 0 })
        );
        assert_eq!(parse_jump_target(r#"{"kind":"page","page":5000000000}"#), Ok(JumpTarget::Page { page: u32::MAX }));
        assert_eq!(
            parse_jump_target(r#"{"kind":"text","text":"x","page":5000000000,"occurrence":9007199254740991}"#),
            Ok(JumpTarget::Text { text: "x".into(), page: Some(u32::MAX), occurrence: u32::MAX })
        );
        for bad in [
            r#"{"kind":"page","page":1.5}"#,
            r#"{"kind":"text","text":"x","page":null,"occurrence":-1}"#,
            r#"{"kind":"text","text":"x","page":1.5,"occurrence":0}"#,
            r#"{"kind":"page","page":-1}"#,
            r#"{"kind":"text","text":"x","page":null}"#,
            r#"{"kind":"chapter","page":1}"#,
            r#""page 1""#,
        ] {
            assert!(parse_jump_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn marks_need_a_colour_and_a_bounded_count() {
        let marks =
            parse_marks(r##"[{"id":"t1","text":"Goods","page":null,"color":"#FF000080"},{"id":"t2","text":"x","page":2,"color":"#00ff00"}]"##)
                .expect("valid");
        assert_eq!(marks[0].color, Color32::from_rgba_unmultiplied(255, 0, 0, 128));
        assert_eq!(marks[1].page, Some(2));
        assert_eq!(marks[1].color, Color32::from_rgb(0, 255, 0));
        assert_eq!(parse_marks("[]"), Ok(Vec::new()));
        let err = parse_marks(r#"[{"id":"t1","text":"x","page":null,"color":"red"}]"#).expect_err("not a colour");
        assert!(err.contains("t1"), "{err}");
        let many_marks: Vec<String> =
            (0..=MAX_MARKS).map(|index| format!(r##"{{"id":"m{index}","text":"x","page":null,"color":"#000000ff"}}"##)).collect();
        let many = format!("[{}]", many_marks.join(","));
        let kept = parse_marks(&many).expect("marks beyond the cap are ignored, not refused");
        assert_eq!(kept.len(), MAX_MARKS);
        assert_eq!(kept[0].id, "m0");
        assert_eq!(kept[MAX_MARKS - 1].id, format!("m{}", MAX_MARKS - 1));
    }

    #[test]
    fn mount_options_take_view_mode_and_six_digit_colours() {
        let theme = parse_mount_options(r##"{"mode":"view","theme":{"dark":true,"accent":"#0f766e","accentContent":"#FFFFFF"}}"##).expect("valid");
        assert_eq!(theme, Theme { dark: true, accent: Color32::from_rgb(0x0f, 0x76, 0x6e), accent_content: Color32::WHITE });
        assert!(parse_mount_options(r##"{"mode":"edit","theme":{"dark":true,"accent":"#0f766e","accentContent":"#ffffff"}}"##).is_err());
        for accent in ["#0f766", "0f766e", "#0f766e80", "#0g766e"] {
            let json = format!(r##"{{"dark":false,"accent":"{accent}","accentContent":"#ffffff"}}"##);
            assert!(parse_theme(&json).is_err(), "{accent}");
        }
    }

    #[test]
    fn events_have_the_api_shapes() {
        assert_eq!(shape(&HostEvent::Opened { pages: 3 }), json!({"type": "opened", "pages": 3}));
        assert_eq!(shape(&HostEvent::OpenFailed { message: "m".into() }), json!({"type": "openFailed", "message": "m"}));
        assert_eq!(shape(&HostEvent::PageChanged { page: 2 }), json!({"type": "pageChanged", "page": 2}));
        assert_eq!(
            shape(&HostEvent::Selection { text: "a b".into(), copy_text: "a\nb".into(), page: Some(1), x: 10.0, y: 20.5 }),
            json!({"type": "selection", "text": "a b", "copyText": "a\nb", "page": 1, "x": 10.0, "y": 20.5})
        );
        assert_eq!(shape(&HostEvent::SelectionCleared), json!({"type": "selectionCleared"}));
        assert_eq!(shape(&HostEvent::MarkActivated { id: "t1".into() }), json!({"type": "markActivated", "id": "t1"}));
        assert_eq!(
            shape(&HostEvent::JumpNotFound { target: JumpTarget::Text { text: "x".into(), page: None, occurrence: 0 } }),
            json!({"type": "jumpNotFound", "target": {"kind": "text", "text": "x", "page": null, "occurrence": 0}})
        );
    }

    #[test]
    fn the_bridge_queues_in_order_and_wakes_the_app() {
        let bridge = HostBridge::default();
        let woken = Arc::new(AtomicUsize::new(0));
        let ctx = egui::Context::default();
        let counter = woken.clone();
        ctx.set_request_repaint_callback(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 1 }));
        assert_eq!(woken.load(Ordering::SeqCst), 0, "no app has run a frame yet");
        bridge.attach(&ctx);
        bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 2 }));
        assert!(woken.load(Ordering::SeqCst) > 0);
        let pages: Vec<u32> = bridge
            .take_commands()
            .into_iter()
            .map(|c| match c {
                HostCommand::JumpTo(JumpTarget::Page { page }) => page,
                _ => 0,
            })
            .collect();
        assert_eq!(pages, [1, 2]);
        assert!(bridge.take_commands().is_empty());
        bridge.emit(HostEvent::SelectionCleared);
        bridge.clone().emit(HostEvent::PageChanged { page: 1 });
        assert_eq!(bridge.take_events(), [HostEvent::SelectionCleared, HostEvent::PageChanged { page: 1 }]);
        assert!(bridge.take_events().is_empty());
    }
}
