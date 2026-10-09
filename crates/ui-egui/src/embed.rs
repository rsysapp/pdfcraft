//! The embed build's host bridge (apps/pdfcraft-embed): a host page drives the page view through
//! commands and hears back through events, and the app runs in view mode. Nothing here touches
//! the browser, so the whole bridge runs under egui_kittest.
//!
//! View mode shows the document area with the page view's own navigation, zoom and find. Changes
//! are refused in the engine (`Session::set_view_only`) and in command dispatch, not only hidden.

mod protocol;
mod rail;
pub mod text_match;

pub use protocol::{
    HostBridge, HostCommand, HostEvent, JumpTarget, MAX_MARKS, Mark, Theme, parse_color, parse_jump_target, parse_marks, parse_mount_options,
    parse_theme,
};

use std::sync::Arc;

use crate::{Mode, PdfCraftApp, QuickTool};

/// What `open` answers for a file that needs a password; view mode never prompts.
const PASSWORD_MESSAGE: &str = "This file is password-protected and can't be shown here.";

/// What `open_bytes` answers in view mode: only the host opens documents, never a drop or a picker.
pub(crate) const OPEN_REFUSED: &str = "only the host page opens documents here";

/// The commands view mode runs: find, page display and zoom. Everything else (editing, saving,
/// printing, exporting, opening, panels, help and its links) is refused.
const VIEW_COMMANDS: &[&str] = &[
    "edit.find",
    "view.layout.continuous",
    "view.layout.single",
    "view.layout.two_up",
    "view.layout.cover",
    "view.fit_width_scrolling",
    "view.fit_one_page",
    "view.fit_visible",
];

/// The app's embed state ([`PdfCraftApp::embedded`]).
#[derive(Default)]
pub(crate) struct Embed {
    bridge: HostBridge,
}

impl PdfCraftApp {
    /// An app driven by a host page through `bridge`, in view mode: the page view with its
    /// navigation, zoom and find, and nothing that changes, saves, prints or opens files.
    pub fn embedded(bridge: HostBridge) -> Self {
        let mut app = Self::new();
        app.session.set_view_only(true);
        // Document scripts never run in view mode: they could open addresses or change fields.
        app.session.set_javascript(false);
        app.default_mode = Mode::Read;
        app.embed = Some(Embed { bridge });
        app.enforce_view_profile();
        app
    }

    /// Whether command `command` may run: every command outside view mode, the viewing ones in it.
    pub(crate) fn embed_allows(&self, command: &str) -> bool {
        self.embed.is_none() || VIEW_COMMANDS.contains(&command)
    }

    /// Run the host's commands (end of `logic`, after the app's own styling and renders).
    pub(crate) fn embed_logic(&mut self, ctx: &egui::Context) {
        let Some(mut embed) = self.embed.take() else { return };
        embed.bridge.attach(ctx);
        for command in embed.bridge.take_commands() {
            match command {
                HostCommand::Open { name, bytes } => self.host_open(&mut embed, &name, bytes),
                HostCommand::JumpTo(target) => embed.bridge.emit(HostEvent::JumpNotFound { target }),
                HostCommand::SetMarks(_) | HostCommand::SetTheme(_) => {}
            }
        }
        // `execute` refuses full screen; should anything still set it, leave it the way the app does,
        // so the window follows.
        if self.full_screen {
            self.set_full_screen(ctx, false);
        }
        self.enforce_view_profile();
        self.embed = Some(embed);
    }

    /// Open the host's file in place of the current one: `opened`, or `openFailed` for a broken
    /// file or one that needs a password.
    fn host_open(&mut self, embed: &mut Embed, name: &str, bytes: Vec<u8>) {
        while !self.views.is_empty() {
            self.close_tab(0);
        }
        let event = match self.try_open(name, None, Arc::new(bytes), None) {
            Err(e) => HostEvent::OpenFailed { message: format!("This file can't be shown: {e}") },
            Ok(()) if self.password_prompt.take().is_some() => HostEvent::OpenFailed { message: PASSWORD_MESSAGE.into() },
            Ok(()) => HostEvent::Opened { pages: self.active_ids().and_then(|(_, id)| self.session.get(id)).map_or(0, |d| d.info.pages.len()) },
        };
        embed.bridge.emit(event);
    }

    /// Keep view mode whatever ran this frame (a notice button, a link, a key): no panel, dialog,
    /// palette or prompt, and no tool beyond selecting text and panning.
    fn enforce_view_profile(&mut self) {
        self.mode = Mode::Read;
        self.left_open = false;
        self.right = None;
        self.dialog = None;
        self.palette_open = false;
        self.close_request = None;
        self.password_prompt = None;
        self.pending_link = None;
        self.full_screen = false;
        if !matches!(self.quick_tool, QuickTool::Select | QuickTool::Hand) {
            self.quick_tool = QuickTool::Select;
        }
        for view in &mut self.views {
            view.organize = false;
        }
    }
}

/// The embed build's frame: the navigation rail and the page view. Dialogs, panels, the palette
/// and notices are never drawn.
pub(crate) fn show(app: &mut PdfCraftApp, ui: &mut egui::Ui) {
    let t = crate::theme::Tokens::get(ui.ctx());
    if app.active.is_some() {
        rail::show(app, ui);
    }
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.pasteboard)).show(ui, |ui| {
        if let Some(i) = app.active {
            crate::canvas::document_area(app, i, ui);
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn view_commands_are_registered_commands() {
        for id in super::VIEW_COMMANDS {
            assert!(pdfcraft_engine::commands::command(id).is_some(), "{id} is a registered command");
        }
    }

    /// A folder for one test that must stay empty.
    fn empty_folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pdfcraft-embed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the folder");
        dir
    }

    #[test]
    fn file_pickers_never_show() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut app = crate::PdfCraftApp::embedded(super::HostBridge::default());
        // The picker's answer when it is allowed to ask (tests and automation).
        app.pick_override = Some(vec![empty_folder("pick").join("chosen.pdf").to_string_lossy().into_owned()]);
        let answered = std::sync::Arc::new(AtomicBool::new(false));
        let flag = answered.clone();
        let asked = app.ask(crate::pickers::Ask::File(rfd::AsyncFileDialog::new()), None, move |_, _| flag.store(true, Ordering::SeqCst));
        app.process_picked();
        assert!(!asked && !answered.load(Ordering::SeqCst));
    }

    #[test]
    fn exports_and_images_are_never_written() {
        let dir = empty_folder("write");
        let mut app = crate::PdfCraftApp::embedded(super::HostBridge::default());
        app.pick_override = Some(vec![dir.to_string_lossy().into_owned()]);
        app.write_files(&[("export.txt".into(), std::sync::Arc::new(b"text".to_vec()))], "Export");
        app.process_picked();
        assert!(!dir.join("export.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
