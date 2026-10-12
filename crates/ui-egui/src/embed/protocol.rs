//! The host bridge's messages (the embed API's commands and events) and the queues that carry them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::Color32;

/// Most marks one `setMarks` keeps; the rest are ignored.
pub const MAX_MARKS: usize = 2_000;

/// Largest file `open` takes. A larger one is answered `openFailed` ([`OPEN_TOO_LARGE`]) without
/// its bytes ever being copied into WebAssembly memory: a failed allocation aborts the module, and
/// that memory never shrinks.
pub const MAX_OPEN_BYTES: usize = 100 << 20;

/// What `open` answers for a file over [`MAX_OPEN_BYTES`] (the same words as the Word editor's).
pub const OPEN_TOO_LARGE: &str = "This file is too large or complex to be shown here.";

/// Whether a file of `len` bytes is refused before anything reads it.
pub fn open_too_large(len: usize) -> bool {
    len > MAX_OPEN_BYTES
}

/// The command for a host's `open` of a file of `len` bytes. The size is checked first, so an
/// oversized file is always "too large". `copy` brings the bytes over and is called only for a file
/// `open` takes that has bytes: a refused or empty one is never read (an empty one is answered
/// `openFailed` as a broken file). The refusal still goes through the queue, so every open is
/// answered once, in the order the host made them. An `Err` from `copy` queues nothing.
pub fn open_command<E>(name: String, len: usize, copy: impl FnOnce() -> Result<Vec<u8>, E>) -> Result<HostCommand, E> {
    if open_too_large(len) {
        return Ok(HostCommand::OpenTooLarge { len });
    }
    let bytes = if len == 0 { Vec::new() } else { copy()? };
    Ok(HostCommand::Open { name, bytes })
}

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

/// A non-negative whole JSON number, saturated to `u32::MAX`: any size is merely out of range for a
/// document (the jump answers not found, the mark is skipped), not malformed. That includes digits
/// beyond `u64` and the exponent form (`1e+21`) that `JSON.stringify` writes for integers from 1e21,
/// so the value is read as a number, not as an integer type. Negatives, fractions and non-numbers
/// stay errors; `3.0` is the whole number 3.
fn saturating_u32<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let number = <serde_json::Number as serde::Deserialize>::deserialize(deserializer)?;
    whole_number_saturated(&number).map_err(serde::de::Error::custom)
}

fn saturating_optional_u32<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let number = <Option<serde_json::Number> as serde::Deserialize>::deserialize(deserializer)?;
    number.map(|number| whole_number_saturated(&number).map_err(serde::de::Error::custom)).transpose()
}

fn whole_number_saturated(number: &serde_json::Number) -> Result<u32, String> {
    if let Some(unsigned) = number.as_u64() {
        return Ok(u32::try_from(unsigned).unwrap_or(u32::MAX));
    }
    // Remaining: negative integers, and anything written with a fraction or exponent (or beyond u64).
    match number.as_f64() {
        // `as` saturates for floats, so values past u32 (and past u64) land on u32::MAX.
        Some(float) if float.is_finite() && float.fract() == 0.0 && float >= 0.0 => Ok(float as u32),
        _ => Err(format!("{number} is not a whole number of zero or more")),
    }
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
    Open {
        name: String,
        bytes: Vec<u8>,
    },
    /// An `open` refused before its bytes were copied ([`open_command`]): only its length came over.
    OpenTooLarge {
        len: usize,
    },
    JumpTo(JumpTarget),
    SetMarks(Vec<Mark>),
    SetTheme(Theme),
    /// Embed API 0.2: view or edit mode.
    SetMode(ModeRequest),
    RequestSave,
    /// Kept for API stability; nothing is ever applied in the browser (R6b-49).
    ApplyRedactions,
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
    ModeChanged {
        mode: EditorMode,
    },
    DirtyChanged {
        dirty: bool,
    },
    /// `bytes` is set on the JavaScript object by the embed crate, outside JSON.
    SaveRequested {
        #[serde(rename = "fileName")]
        file_name: String,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    SaveFailed {
        message: String,
    },
    CommentRequested {
        text: String,
        #[serde(rename = "copyText")]
        copy_text: String,
        page: Option<usize>,
        x: f32,
        y: f32,
    },
    RedactionsPending {
        marks: usize,
    },
    /// An action edit mode refused, by a stable code the host words itself.
    EditRefused {
        code: &'static str,
    },
}

impl HostEvent {
    /// The event as JSON, the shape the host's listener receives.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }
}

/// The codes `editRefused` carries that this editor sends (embed API 0.2).
pub mod refusal {
    /// A tool or command the permissions or edit mode do not offer.
    pub const COMMAND_NOT_ALLOWED: &str = "command_not_allowed";
    /// A `setMode` that would mix two authors or two dates in one save.
    pub const INVALID_EDIT_SESSION: &str = "invalid_edit_session";
    /// A command this editor does not know.
    pub const UNKNOWN_COMMAND: &str = "unknown_command";
    /// An allowed change the engine could not make (a page that does not exist, …).
    pub const COMMAND_FAILED: &str = "command_failed";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EditorMode {
    View,
    Edit,
}

/// `setMode`'s request (embed API 0.2).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum ModeRequest {
    View,
    Edit {
        author: EditAuthor,
        #[serde(rename = "revisionDate")]
        revision_date: String,
        permissions: EditPermissions,
    },
}

/// The person editing. Fields of `author` this editor does not read are ignored (embed API 0.2).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct EditAuthor {
    pub id: String,
    pub name: String,
}

/// Which tools edit mode offers; this editor ignores `accept_reject_changes`. An unknown
/// permission is refused, never ignored: a restriction the editor cannot read must not become a
/// tool it offers (Ruling W5-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditPermissions {
    pub accept_reject_changes: bool,
    pub organize_pages: bool,
    pub fill_forms: bool,
    pub redact: bool,
    pub stamps: bool,
    pub drawing: bool,
}

/// `YYYY-MM-DDTHH:MM:SS`, an optional fraction, `Z`.
fn is_utc_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes.get(range).is_some_and(|part| part.iter().all(u8::is_ascii_digit));
    let at = |index: usize, expected: u8| bytes.get(index) == Some(&expected);
    let head = digits(0..4)
        && at(4, b'-')
        && digits(5..7)
        && at(7, b'-')
        && digits(8..10)
        && at(10, b'T')
        && digits(11..13)
        && at(13, b':')
        && digits(14..16)
        && at(16, b':')
        && digits(17..19);
    let tail = match bytes.get(19..) {
        Some([b'Z']) => true,
        Some([b'.', rest @ ..]) => {
            rest.split_last().is_some_and(|(last, fraction)| *last == b'Z' && !fraction.is_empty() && fraction.iter().all(u8::is_ascii_digit))
        }
        _ => false,
    };
    head && tail
}

/// Seconds since the Unix epoch of a timestamp `is_utc_timestamp` accepts (the fraction dropped),
/// or `None` for a date or time that does not exist on the calendar (2026-02-31, 25:00).
pub(crate) fn unix_seconds(text: &str) -> Option<i64> {
    if !is_utc_timestamp(text) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm; `pdfcraft_cos::pdf_date` runs it the
    // other way).
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// `ModeRequest` from the host's JSON: an author name of 1 to 255 characters and an ISO 8601 UTC
/// `revisionDate` that exists on the calendar.
pub fn parse_mode_request(json: &str) -> Result<ModeRequest, String> {
    let request: ModeRequest = serde_json::from_str(json).map_err(|e| format!("setMode: {e}"))?;
    if let ModeRequest::Edit { author, revision_date, .. } = &request {
        if author.name.trim().is_empty() || author.name.chars().count() > 255 {
            return Err("setMode: author.name must be 1 to 255 characters".into());
        }
        if unix_seconds(revision_date).is_none() {
            return Err("setMode: revisionDate must be an ISO 8601 UTC timestamp".into());
        }
    }
    Ok(request)
}

/// The two queues between a host page and the running app, shared by both sides. A poisoned lock
/// still holds valid queues, so its contents are used rather than dropped.
#[derive(Clone, Default)]
pub struct HostBridge {
    commands: Arc<Mutex<VecDeque<HostCommand>>>,
    events: Arc<Mutex<Vec<HostEvent>>>,
    /// The app's context once it has run a frame, so a command wakes an idle app.
    ctx: Arc<Mutex<Option<egui::Context>>>,
    /// Set by [`HostBridge::detach`]; read under each queue's lock, so nothing slips in after it.
    detached: Arc<AtomicBool>,
}

impl HostBridge {
    /// Host side: queue a command; the app runs it on its next frame, which this requests.
    pub fn send(&self, command: HostCommand) {
        {
            let mut commands = self.commands.lock().unwrap_or_else(PoisonError::into_inner);
            if self.detached.load(Ordering::SeqCst) {
                return;
            }
            commands.push_back(command);
        }
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
        if slot.is_none() && !self.detached.load(Ordering::SeqCst) {
            *slot = Some(ctx.clone());
        }
    }

    /// App side: every command since the last call, oldest first.
    pub fn take_commands(&self) -> Vec<HostCommand> {
        self.commands.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect()
    }

    /// App side: tell the host something.
    pub fn emit(&self, event: HostEvent) {
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.detached.load(Ordering::SeqCst) {
            events.push(event);
        }
    }

    /// Host side, when the editor is destroyed: drops the app's context and both queues, and
    /// makes every later call carry nothing. The context would otherwise outlive the stopped
    /// app through the host's copy of the bridge. The flag is set before each lock is taken, so a
    /// call that takes the lock after this clears it sees the flag.
    pub fn detach(&self) {
        self.detached.store(true, Ordering::SeqCst);
        // Dropped after our lock is released, as in `send`: the last copy takes egui's state with it.
        let context = self.ctx.lock().unwrap_or_else(PoisonError::into_inner).take();
        drop(context);
        self.commands.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.events.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

#[derive(serde::Deserialize)]
struct WireMark {
    id: String,
    text: String,
    // Saturated like a jump's page: one beyond u32 is a page no document has, so the mark is skipped.
    #[serde(default, deserialize_with = "saturating_optional_u32")]
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
    fn any_whole_host_number_saturates_and_the_rest_is_refused() {
        // Digits beyond u64 and exponent forms (what JSON.stringify writes from 1e21) saturate.
        for huge in ["18446744073709551615", "18446744073709551616", "100000000000000000000", "1e+21", "1e300", "4294967296", "1.5e10"] {
            let target = parse_jump_target(&format!(r#"{{"kind":"page","page":{huge}}}"#)).unwrap_or_else(|e| panic!("{huge}: {e}"));
            assert_eq!(target, JumpTarget::Page { page: u32::MAX }, "{huge}");
            let text = format!(r#"{{"kind":"text","text":"x","page":{huge},"occurrence":{huge}}}"#);
            assert_eq!(parse_jump_target(&text), Ok(JumpTarget::Text { text: "x".into(), page: Some(u32::MAX), occurrence: u32::MAX }), "{huge}");
            let marks = format!(r##"[{{"id":"m","text":"x","page":{huge},"color":"#000000ff"}}]"##);
            assert_eq!(parse_marks(&marks).expect("saturates")[0].page, Some(u32::MAX), "{huge}");
        }
        assert_eq!(parse_jump_target(r#"{"kind":"page","page":3.0}"#), Ok(JumpTarget::Page { page: 3 }));
        assert_eq!(parse_jump_target(r#"{"kind":"page","page":4294967295}"#), Ok(JumpTarget::Page { page: u32::MAX }));
        for bad in ["-1", "1.5", "-0.5", "\"2\"", "-1e300", "true"] {
            assert!(parse_jump_target(&format!(r#"{{"kind":"page","page":{bad}}}"#)).is_err(), "{bad}");
            let text = format!(r#"{{"kind":"text","text":"x","page":null,"occurrence":{bad}}}"#);
            assert!(parse_jump_target(&text).is_err(), "occurrence {bad}");
            let marks = format!(r##"[{{"id":"m","text":"x","page":{bad},"color":"#000000ff"}}]"##);
            assert!(parse_marks(&marks).is_err(), "mark {bad}");
        }
        // An omitted page still means "no page".
        assert_eq!(
            parse_jump_target(r#"{"kind":"text","text":"x","occurrence":0}"#),
            Ok(JumpTarget::Text { text: "x".into(), page: None, occurrence: 0 })
        );
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
        // A page beyond u32 is out of range for any document (the mark is skipped), not malformed.
        let far = parse_marks(r##"[{"id":"far","text":"x","page":5000000000,"color":"#000000ff"}]"##).expect("saturates");
        assert_eq!(far[0].page, Some(u32::MAX));
        for bad_page in ["-1", "1.5", "\"2\""] {
            let json = format!(r##"[{{"id":"t","text":"x","page":{bad_page},"color":"#000000ff"}}]"##);
            assert!(parse_marks(&json).is_err(), "{bad_page}");
        }
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

    #[test]
    fn a_detached_bridge_frees_the_context_and_carries_nothing() {
        let bridge = HostBridge::default();
        let woken = Arc::new(AtomicUsize::new(0));
        let ctx = egui::Context::default();
        let counter = woken.clone();
        // The callback lives as long as the context, so its clone of `woken` shows whether the
        // context is still alive.
        ctx.set_request_repaint_callback(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        bridge.attach(&ctx);
        drop(ctx);
        assert_eq!(Arc::strong_count(&woken), 2, "the bridge keeps the context alive");
        bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 1 }));
        bridge.emit(HostEvent::SelectionCleared);
        let app_side = bridge.clone();

        bridge.detach();
        assert_eq!(Arc::strong_count(&woken), 1, "detaching drops the context");
        assert!(app_side.take_commands().is_empty(), "queued commands are dropped");
        assert!(bridge.take_events().is_empty(), "queued events are dropped");

        let woken_before = woken.load(Ordering::SeqCst);
        bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 2 }));
        app_side.emit(HostEvent::PageChanged { page: 2 });
        let late = egui::Context::default();
        app_side.attach(&late);
        bridge.send(HostCommand::JumpTo(JumpTarget::Page { page: 3 }));
        assert_eq!(woken.load(Ordering::SeqCst), woken_before);
        assert!(app_side.take_commands().is_empty(), "a detached bridge queues no command");
        assert!(bridge.take_events().is_empty(), "a detached bridge queues no event");
        assert!(app_side.ctx.lock().map(|slot| slot.is_none()).unwrap_or(false), "a detached bridge keeps no context");
        bridge.detach();
    }
}
