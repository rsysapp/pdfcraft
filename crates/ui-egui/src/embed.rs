//! The embed build's host bridge (apps/pdfcraft-embed): a host page drives the page view through
//! commands and hears back through events, and the app runs in view mode. Nothing here touches
//! the browser, so the whole bridge runs under egui_kittest.

mod protocol;
pub mod text_match;

pub use protocol::{
    HostBridge, HostCommand, HostEvent, JumpTarget, MAX_MARKS, Mark, Theme, parse_color, parse_jump_target, parse_marks, parse_mount_options,
    parse_theme,
};
