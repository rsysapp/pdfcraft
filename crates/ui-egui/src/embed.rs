//! The embed build's host bridge (apps/pdfcraft-embed): a host page drives the page view through
//! commands and hears back through events, and the app runs in view mode. Nothing here touches
//! the browser, so the whole bridge runs under egui_kittest.
//!
//! View mode shows the document area with the page view's own navigation, zoom and find. Changes
//! are refused in the engine (`Session::set_view_only`) and in command dispatch, not only hidden.

mod host_theme;
mod protocol;
mod rail;
pub mod text_match;

pub use protocol::{
    HostBridge, HostCommand, HostEvent, JumpTarget, MAX_MARKS, Mark, Theme, parse_color, parse_jump_target, parse_marks, parse_mount_options,
    parse_theme,
};

use std::ops::Range;
use std::sync::Arc;

use pdfcraft_render::DocInfo;

use crate::theme::{ThemeKind, ThemePreference};
use crate::{DocView, Mode, PdfCraftApp, QuickTool};

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
    /// A theme to apply once this frame's styling has run.
    theme: Option<Theme>,
    /// A text jump waiting for the text layers it searches.
    jump: Option<JumpTarget>,
    /// The page last reported (`pageChanged`), 0-based.
    last_page: Option<usize>,
    /// The selection last reported, as (page, glyph range).
    last_selection: Option<(usize, Range<usize>)>,
}

impl Embed {
    /// Forget what belonged to the document that was open.
    fn document_closed(&mut self) {
        self.jump = None;
        self.last_page = None;
        self.last_selection = None;
    }
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
        app.embed = Some(Embed { bridge, ..Embed::default() });
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
                HostCommand::JumpTo(target) => self.host_jump(&mut embed, target),
                HostCommand::SetMarks(_) => {}
                HostCommand::SetTheme(theme) => embed.theme = Some(theme),
            }
        }
        if let Some(theme) = embed.theme.take() {
            self.apply_host_theme(ctx, &theme);
        }
        // Text layers that arrived this frame (`receive`, earlier in `logic`).
        self.resolve_jump(&mut embed);
        // A page-scoped jump needs only that page's text; an unscoped one searches every page.
        let pages = self.active_page_count();
        let (all_text, text_page) = match &embed.jump {
            Some(JumpTarget::Text { page: Some(page), .. }) => (false, page_index(*page, pages)),
            Some(_) => (true, None),
            None => (false, None),
        };
        if let Some(view) = self.active.and_then(|i| self.views.get_mut(i)) {
            view.want_all_text = all_text;
            view.want_text_page = text_page;
        }
        // `execute` refuses full screen; should anything still set it, leave it the way the app does,
        // so the window follows.
        if self.full_screen {
            self.set_full_screen(ctx, false);
        }
        self.enforce_view_profile();
        self.embed = Some(embed);
    }

    /// Whether host work (a text jump) still waits for text layers (tests).
    pub fn host_work_pending(&self) -> bool {
        self.embed.as_ref().is_some_and(|e| e.jump.is_some())
    }

    fn active_page_count(&self) -> usize {
        self.active_ids().and_then(|(_, id)| self.session.get(id)).map_or(0, |d| d.info.pages.len())
    }

    /// Start a jump: a page goes at once; text resolves now if its text layers are in, otherwise
    /// it waits (a newer jump replaces one still waiting).
    fn host_jump(&mut self, embed: &mut Embed, target: JumpTarget) {
        // Any newer jump, found or not, replaces one still waiting for text layers.
        embed.jump = None;
        let pages = self.active_page_count();
        let nowhere = match &target {
            JumpTarget::Page { page } => match (page_index(*page, pages), self.active.and_then(|i| self.views.get_mut(i))) {
                (Some(p), Some(view)) => {
                    view.go_to_page(p);
                    false
                }
                _ => true,
            },
            JumpTarget::Text { page, .. } => pages == 0 || page.is_some_and(|p| page_index(p, pages).is_none()),
        };
        if nowhere {
            embed.bridge.emit(HostEvent::JumpNotFound { target });
        } else if matches!(target, JumpTarget::Text { .. }) {
            embed.jump = Some(target);
            self.resolve_jump(embed);
        }
    }

    /// Finish a waiting text jump once every page it searches has its text layer.
    fn resolve_jump(&mut self, embed: &mut Embed) {
        let Some(JumpTarget::Text { text, page, occurrence }) = &embed.jump else { return };
        let Some((index, id)) = self.active_ids() else { return };
        let (Some(doc), Some(view)) = (self.session.get(id), self.views.get_mut(index)) else { return };
        let pages = doc.info.pages.len();
        let scope: Vec<usize> = match page.and_then(|p| page_index(p, pages)) {
            Some(p) => vec![p],
            None => (0..pages).collect(),
        };
        if !scope.iter().all(|p| view.texts.contains_key(p) || view.text_failed.contains(p)) {
            return;
        }
        let needle = [text_match::fold(text)];
        let wanted = usize::try_from(*occurrence).unwrap_or(usize::MAX);
        let hit = scope
            .iter()
            .flat_map(|&p| {
                let found = view.texts.get(&p).map(|t| text_match::find_all(t, &needle)).unwrap_or_default();
                found.into_iter().flatten().map(move |r| (p, r))
            })
            .nth(wanted);
        let found = hit.is_some_and(|(p, range)| jump_to_match(view, &doc.info, p, range));
        match embed.jump.take() {
            Some(target) if !found => embed.bridge.emit(HostEvent::JumpNotFound { target }),
            _ => {}
        }
    }

    /// The host's theme, recorded as an explicit preference so the app's system-theme sync
    /// (`sync_theme`) never replaces it.
    fn apply_host_theme(&mut self, ctx: &egui::Context, theme: &Theme) {
        self.theme = if theme.dark { ThemeKind::Dark } else { ThemeKind::Light };
        self.theme_preference = if theme.dark { ThemePreference::Dark } else { ThemePreference::Light };
        host_theme::apply(ctx, theme);
    }

    /// Open the host's file in place of the current one: `opened`, or `openFailed` for a broken
    /// file or one that needs a password.
    fn host_open(&mut self, embed: &mut Embed, name: &str, bytes: Vec<u8>) {
        while !self.views.is_empty() {
            self.close_tab(0);
        }
        embed.document_closed();
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

/// A host page number (1-based) as a page index, if the document has that page.
fn page_index(page: u32, pages: usize) -> Option<usize> {
    usize::try_from(page).ok()?.checked_sub(1).filter(|p| *p < pages)
}

/// Bring a match into view as the find bar does, and outline it briefly. `false` when the page's
/// text or the match's lines are unexpectedly missing, so the host hears `jumpNotFound`.
fn jump_to_match(view: &mut DocView, info: &DocInfo, page: usize, range: Range<usize>) -> bool {
    let Some(text) = view.texts.get(&page).cloned() else { return false };
    let rects = text.line_rects(range);
    let Some(first) = rects.first().copied() else { return false };
    let bounds = rects.iter().fold(first, |a, r| [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[2]), a[3].max(r[3])]);
    let height = info.pages.get(page).map_or(1.0, |p| p.height.max(1.0));
    view.go_to_page(page);
    view.goto = Some((page, ((first[1] - 60.0) / height).clamp(0.0, 1.0)));
    if let Some(p) = info.pages.get(page) {
        let (a, b) = (p.view_to_user(bounds[0], bounds[1]), p.view_to_user(bounds[2], bounds[3]));
        view.flash = Some((page, [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])], 0.0));
    }
    true
}

/// What the host hears about the frame just drawn.
fn collect_events(app: &mut PdfCraftApp, ctx: &egui::Context) {
    let Some(embed) = app.embed.as_mut() else { return };
    let Some(view) = app.active.and_then(|i| app.views.get(i)) else { return };
    if embed.last_page != Some(view.current) {
        embed.last_page = Some(view.current);
        embed.bridge.emit(HostEvent::PageChanged { page: view.current + 1 });
    }
    // A selection settles when no button is down: drags report on release, keys at once.
    let now = view.selection_span();
    if now == embed.last_selection || ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    match &now {
        None => embed.bridge.emit(HostEvent::SelectionCleared),
        Some((page, range)) => {
            // Off screen (no position yet): try again next frame.
            let Some(event) = selection_event(view, *page, range.clone(), ctx.zoom_factor()) else { return };
            embed.bridge.emit(event);
        }
    }
    embed.last_selection = now;
}

/// The selection as the host hears it, placed at the end of its last line: egui points times the
/// zoom factor are CSS pixels from the canvas's top-left.
fn selection_event(view: &DocView, page: usize, range: Range<usize>, zoom: f32) -> Option<HostEvent> {
    let text = view.texts.get(&page)?;
    let copy_text = text.text_of(range.clone());
    if copy_text.trim().is_empty() {
        return None;
    }
    let last = text.line_rects(range).last().copied()?;
    let end = view.page_xform(page)?.view_rect(last).right_bottom();
    let (x, y) = (end.x * zoom, end.y * zoom);
    let text = copy_text.split_whitespace().collect::<Vec<_>>().join(" ");
    (x.is_finite() && y.is_finite()).then_some(HostEvent::Selection { text, copy_text, page: Some(page + 1), x, y })
}

/// The embed build's frame: the navigation rail and the page view. Dialogs, panels, the palette
/// and notices are never drawn.
pub(crate) fn show(app: &mut PdfCraftApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let t = crate::theme::Tokens::get(&ctx);
    if app.active.is_some() {
        rail::show(app, ui);
    }
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.pasteboard)).show(ui, |ui| {
        if let Some(i) = app.active {
            crate::canvas::document_area(app, i, ui);
        }
    });
    collect_events(app, &ctx);
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
