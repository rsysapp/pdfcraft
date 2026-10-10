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
    HostBridge, HostCommand, HostEvent, JumpTarget, MAX_MARKS, MAX_OPEN_BYTES, Mark, OPEN_TOO_LARGE, Theme, open_command, open_too_large,
    parse_color, parse_jump_target, parse_marks, parse_mount_options, parse_theme,
};

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use pdfcraft_render::{DocInfo, PageText};

use crate::canvas::PageXform;
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
    /// The selection last settled, as (page, glyph range), blank ones included.
    last_selection: Option<(usize, Range<usize>)>,
    /// Whether the host was last told of a selection (`selection`), not of none (`selectionCleared`).
    selection_reported: bool,
    /// The host's marks as it sent them, and their texts folded for matching.
    marks: Vec<Mark>,
    needles: Vec<Vec<char>>,
    /// Each mark's first match, parallel to `marks`: its page and line rectangles. Text layers
    /// arrive in any order, so a hit on a later page is replaced when an earlier page turns one up.
    hits: Vec<Option<(usize, Vec<[f32; 4]>)>>,
    /// The hits as the page view paints them, in the host's mark order (the last one is on top).
    page_marks: Arc<Vec<PageMark>>,
    /// Pages already searched for the current marks.
    searched: HashSet<usize>,
}

/// A host mark's rectangle on one page, in page view space (points, y down).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PageMark {
    id: String,
    page: usize,
    rect: [f32; 4],
    color: egui::Color32,
}

impl Embed {
    /// Forget what belonged to the document that was open, marks included.
    fn document_closed(&mut self) {
        // The host's toolbar belongs to the closed document's selection; tell it before `opened`.
        self.clear_reported_selection(None);
        // A jump still waiting for text belonged to the closed document: answer it with its own
        // target, so the host is never left waiting and it is never run against the next document.
        if let Some(target) = self.jump.take() {
            self.bridge.emit(HostEvent::JumpNotFound { target });
        }
        self.last_page = None;
        self.last_selection = None;
        self.selection_reported = false;
        self.set_marks(Vec::new());
    }

    /// When the editor itself moves the view or replaces the document, a selection the host was
    /// told of is cleared: the host's Comment/Copy toolbar sits at the reported coordinates, which
    /// go stale. Says nothing when none was reported. `view` is the one showing the selection.
    fn clear_reported_selection(&mut self, view: Option<&mut DocView>) {
        if !self.selection_reported {
            return;
        }
        self.bridge.emit(HostEvent::SelectionCleared);
        self.selection_reported = false;
        // The view's own selection goes too, or the host and the view would disagree about it.
        self.last_selection = None;
        if let Some(view) = view {
            view.clear_selection();
        }
    }

    /// Replace every host mark; they are searched for again as text layers arrive. A mark that can
    /// never match (blank once folded, or longer than matching takes, raw or folded) is skipped
    /// here, so it never asks for any page's text.
    fn set_marks(&mut self, marks: Vec<Mark>) {
        let (marks, needles): (Vec<Mark>, Vec<Vec<char>>) = marks
            .into_iter()
            .filter_map(|m| {
                let needle = text_match::fold_passage(&m.text)?;
                Some((m, needle))
            })
            .unzip();
        self.needles = needles;
        self.hits = vec![None; marks.len()];
        self.marks = marks;
        self.page_marks = Arc::default();
        self.searched.clear();
    }

    /// Pages whose text some mark still needs: unsearched pages it may lie on, and for a mark
    /// already found, only those before its hit (a later page cannot change its first match).
    fn pages_pending(&self, pages: usize) -> impl Iterator<Item = usize> + '_ {
        (0..pages).filter(|p| {
            !self.searched.contains(p)
                && self.marks.iter().zip(&self.hits).any(|(m, hit)| mark_may_lie_on(m, *p) && hit.as_ref().is_none_or(|(found, _)| p < found))
        })
    }

    /// Some page a mark may lie on has not been searched yet.
    fn marks_pending(&self, pages: usize) -> bool {
        self.pages_pending(pages).next().is_some()
    }

    /// The text layers the marks need: every page when a mark may lie on any (it has no page),
    /// otherwise the next pending page alone (one at a time, as `want_text_page` takes one).
    fn marks_text_need(&self, pages: usize) -> (bool, Option<usize>) {
        if !self.marks_pending(pages) {
            return (false, None);
        }
        let any_page = self.marks.iter().zip(&self.hits).any(|(m, hit)| m.page.is_none() && hit.is_none());
        if any_page { (true, None) } else { (false, self.pages_pending(pages).next()) }
    }

    /// Search the pages whose text layer arrived since the last call, for every mark that may lie
    /// there. Marks that are not found are skipped.
    fn resolve_marks(&mut self, view: &DocView) {
        if self.marks.is_empty() {
            return;
        }
        let mut ready: Vec<usize> = view.texts.keys().chain(view.text_failed.iter()).copied().filter(|p| !self.searched.contains(p)).collect();
        ready.sort_unstable();
        ready.dedup();
        for page in ready {
            self.search_page(page, view.texts.get(&page).map(|t| &**t));
        }
    }

    /// Search one page's text (`None` when it has none) for the marks that may lie there, and keep
    /// each mark's first match: on its own page when it has one, else on the lowest page.
    fn search_page(&mut self, page: usize, text: Option<&PageText>) {
        self.searched.insert(page);
        let Some(text) = text else { return };
        let here: Vec<usize> = (0..self.marks.len()).filter(|&k| self.marks.get(k).is_some_and(|m| mark_may_lie_on(m, page))).collect();
        let needles: Vec<Vec<char>> = here.iter().filter_map(|&k| self.needles.get(k).cloned()).collect();
        let mut changed = false;
        for (k, ranges) in here.iter().zip(text_match::find_all(text, &needles)) {
            let Some(range) = ranges.into_iter().next() else { continue };
            let Some(hit) = self.hits.get_mut(*k) else { continue };
            if hit.as_ref().is_some_and(|(found, _)| *found <= page) {
                continue;
            }
            *hit = Some((page, text.line_rects(range)));
            changed = true;
        }
        if changed {
            let color_of = |k: usize| self.marks.get(k).map(|m| m.color);
            let found: Vec<PageMark> = (0..self.hits.len())
                .filter_map(|k| Some((k, self.hits.get(k)?.as_ref()?, color_of(k)?)))
                .flat_map(|(k, (page, rects), color)| {
                    let id = self.marks.get(k).map(|m| m.id.clone()).unwrap_or_default();
                    rects.iter().map(move |rect| PageMark { id: id.clone(), page: *page, rect: *rect, color })
                })
                .collect();
            self.page_marks = Arc::new(found);
        }
    }
}

/// Set up the embed build's egui context, once, when the runner creates the app.
pub fn prepare_context(ctx: &egui::Context) {
    // egui sends `ViewportCommand::SetTheme` whenever the theme derived from the preference changes,
    // so a native window's decorations follow it. A canvas has no decorations, and eframe's web
    // backend only logs the command as unhandled, once per host theme change. The explicit theme
    // preference stays (`apply_host_theme`); only the window sync goes.
    ctx.options_mut(|options| options.sync_window_theme = false);
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
                HostCommand::OpenTooLarge { .. } => self.host_open_refused(&mut embed, OPEN_TOO_LARGE),
                HostCommand::JumpTo(target) => self.host_jump(&mut embed, target),
                HostCommand::SetMarks(marks) => embed.set_marks(marks),
                HostCommand::SetTheme(theme) => embed.theme = Some(theme),
            }
        }
        if let Some(theme) = embed.theme.take() {
            self.apply_host_theme(ctx, &theme);
        }
        // Text layers that arrived this frame (`receive`, earlier in `logic`).
        self.resolve_jump(&mut embed);
        if let Some(view) = self.active.and_then(|i| self.views.get(i)) {
            embed.resolve_marks(view);
        }
        // A page-scoped jump needs only that page's text; an unscoped one searches every page.
        let pages = self.active_page_count();
        let (all_text, text_page) = match &embed.jump {
            Some(JumpTarget::Text { page: Some(page), .. }) => (false, page_index(*page, pages)),
            Some(_) => (true, None),
            None => (false, None),
        };
        // Marks without a page need every page's text; scoped ones only their own pages.
        let (marks_all_text, marks_text_page) = embed.marks_text_need(pages);
        if let Some(view) = self.active.and_then(|i| self.views.get_mut(i)) {
            view.want_all_text = all_text || marks_all_text;
            view.want_text_page = text_page.or(marks_text_page);
        }
        // `execute` refuses full screen; should anything still set it, leave it the way the app does,
        // so the window follows.
        if self.full_screen {
            self.set_full_screen(ctx, false);
        }
        self.enforce_view_profile();
        self.embed = Some(embed);
    }

    /// Whether host work (a text jump, marks) still waits for text layers (tests).
    // Public only for the embed build's integration tests; not part of the crate's API.
    #[doc(hidden)]
    pub fn host_work_pending(&self) -> bool {
        let pages = self.active_page_count();
        self.embed.as_ref().is_some_and(|e| e.jump.is_some() || e.marks_pending(pages))
    }

    /// The host marks found: (id, 1-based page, colour), one per line, by page then id (tests).
    // Public only for the embed build's integration tests; not part of the crate's API.
    #[doc(hidden)]
    pub fn found_host_marks(&self) -> Vec<(String, usize, egui::Color32)> {
        let mut out: Vec<(String, usize, egui::Color32)> =
            self.embed.iter().flat_map(|e| e.page_marks.iter().map(|m| (m.id.clone(), m.page + 1, m.color))).collect();
        out.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        out
    }

    /// Where the host marks are on screen this frame: (id, rectangle) (tests and automation).
    // Public only for the embed build's integration tests; not part of the crate's API.
    #[doc(hidden)]
    pub fn host_mark_rects(&self) -> Vec<(String, egui::Rect)> {
        let (Some(embed), Some(view)) = (self.embed.as_ref(), self.active.and_then(|i| self.views.get(i))) else { return Vec::new() };
        embed.page_marks.iter().filter_map(|m| view.page_xform(m.page).map(|xf| (m.id.clone(), xf.view_rect(m.rect)))).collect()
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
                    embed.clear_reported_selection(Some(view));
                    false
                }
                _ => true,
            },
            // An over-long passage is answered now: it must not wait for text layers it can never match.
            JumpTarget::Text { text, page, .. } => {
                pages == 0 || page.is_some_and(|p| page_index(p, pages).is_none()) || text_match::exceeds_raw_cap(text)
            }
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
        // A jump that finds nothing leaves the view, and so a reported selection, where it was.
        if found {
            embed.clear_reported_selection(Some(view));
        }
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
        // The embed crate refuses these before copying them; a native caller may still send one.
        if open_too_large(bytes.len()) {
            return self.host_open_refused(embed, OPEN_TOO_LARGE);
        }
        self.close_for_host_open(embed);
        let event = match self.try_open(name, None, Arc::new(bytes), None) {
            Err(e) => HostEvent::OpenFailed { message: format!("This file can't be shown: {e}") },
            Ok(()) if self.password_prompt.take().is_some() => HostEvent::OpenFailed { message: PASSWORD_MESSAGE.into() },
            Ok(()) => HostEvent::Opened { pages: self.active_ids().and_then(|(_, id)| self.session.get(id)).map_or(0, |d| d.info.pages.len()) },
        };
        embed.bridge.emit(event);
    }

    /// An open refused before its file was read: answered `openFailed` in its place among the
    /// answers, and nothing else changes (the open document, its marks, a waiting jump and a
    /// reported selection all stay), as in the Word editor.
    fn host_open_refused(&mut self, embed: &mut Embed, message: &str) {
        embed.bridge.emit(HostEvent::OpenFailed { message: message.into() });
    }

    /// Close the open document for the host's next one (`selectionCleared` and a waiting jump's
    /// `jumpNotFound` go first).
    fn close_for_host_open(&mut self, embed: &mut Embed) {
        while !self.views.is_empty() {
            self.close_tab(0);
        }
        embed.document_closed();
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

/// Whether `mark` may lie on page index `page`.
fn mark_may_lie_on(mark: &Mark, page: usize) -> bool {
    mark.page.is_none_or(|p| page_index(p, usize::MAX) == Some(page))
}

/// The host marks for the page view to paint (none outside the embed build).
pub(crate) fn page_marks(embed: Option<&Embed>) -> Arc<Vec<PageMark>> {
    embed.map(|e| e.page_marks.clone()).unwrap_or_default()
}

/// Paint the host marks on `page` (above the page, below the selection) and note the one a click
/// this frame landed in. Marks paint in the order the host sent them, so a click where marks
/// overlap goes to the last one: the one drawn on top. A drag is never a click, so selecting text
/// across a mark does not activate it.
pub(crate) fn paint_marks(
    painter: &egui::Painter,
    xf: &PageXform,
    page: usize,
    marks: &[PageMark],
    resp: &egui::Response,
    clicked: &mut Option<String>,
) {
    let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
    for mark in marks.iter().filter(|m| m.page == page) {
        let rect = xf.view_rect(mark.rect);
        painter.rect_filled(rect, egui::CornerRadius::same(2), mark.color);
        if click.is_some_and(|p| rect.contains(p)) {
            *clicked = Some(mark.id.clone());
        }
    }
}

/// A click landed in host mark `id`.
pub(crate) fn mark_activated(app: &PdfCraftApp, id: String) {
    if let Some(embed) = &app.embed {
        embed.bridge.emit(HostEvent::MarkActivated { id });
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
    let Some(view) = app.active.and_then(|i| app.views.get_mut(i)) else { return };
    // The find bar or page buttons moved the view this frame (`moved_by_controls`).
    if std::mem::take(&mut view.moved_by_controls) {
        embed.clear_reported_selection(Some(view));
    }
    if embed.last_page != Some(view.current) {
        embed.last_page = Some(view.current);
        embed.bridge.emit(HostEvent::PageChanged { page: view.current + 1 });
    }
    // A selection settles when no button is down: drags report on release, keys at once.
    let now = view.selection_span();
    if now == embed.last_selection || ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    let reading = match &now {
        None => Reading::Blank,
        Some((page, range)) => selection_event(view, *page, range.clone(), ctx.zoom_factor()),
    };
    match reading {
        // Off screen or its text layer not in yet: try again next frame.
        Reading::NotReady => return,
        // No selection, or only whitespace: nothing for the host to act on, so a selection it was
        // told of is cleared (once), or its toolbar would stay on stale text.
        Reading::Blank => {
            if embed.selection_reported {
                embed.bridge.emit(HostEvent::SelectionCleared);
            }
            embed.selection_reported = false;
        }
        Reading::Text(event) => {
            embed.bridge.emit(event);
            embed.selection_reported = true;
        }
    }
    embed.last_selection = now;
}

/// A settled selection as the host would hear of it.
enum Reading {
    Text(HostEvent),
    /// Only whitespace (or nothing) is selected.
    Blank,
    /// The page is off screen or its text layer is not loaded.
    NotReady,
}

/// The selection as the host hears it, placed at the end of its last line: egui points times the
/// zoom factor are CSS pixels from the canvas's top-left.
fn selection_event(view: &DocView, page: usize, range: Range<usize>, zoom: f32) -> Reading {
    let Some(text) = view.texts.get(&page) else { return Reading::NotReady };
    let copy_text = text.text_of(range.clone());
    if copy_text.trim().is_empty() {
        return Reading::Blank;
    }
    let Some(last) = text.line_rects(range).last().copied() else { return Reading::NotReady };
    let Some(xf) = view.page_xform(page) else { return Reading::NotReady };
    let end = xf.view_rect(last).right_bottom();
    let (x, y) = (end.x * zoom, end.y * zoom);
    let text = copy_text.split_whitespace().collect::<Vec<_>>().join(" ");
    if x.is_finite() && y.is_finite() {
        Reading::Text(HostEvent::Selection { text, copy_text, page: Some(page + 1), x, y })
    } else {
        Reading::NotReady
    }
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
    use super::text_match;

    /// A page with one line of text, one glyph per character.
    fn page_with(line: &str) -> pdfcraft_render::PageText {
        let mut text = pdfcraft_render::PageText::default();
        for (k, ch) in line.chars().enumerate() {
            let x = k as f32 * 7.0;
            text.glyphs.push(pdfcraft_render::TextGlyph { text: ch.to_string(), rect: [x, 0.0, x + 6.0, 12.0] });
            text.line_of.push(0);
            text.space_before.push(ch != ' ' && k > 0 && line.chars().nth(k - 1) == Some(' '));
        }
        text
    }

    fn mark(id: &str, text: &str, page: Option<u32>) -> super::Mark {
        super::Mark { id: id.into(), text: text.into(), page, color: egui::Color32::RED }
    }

    #[test]
    fn a_mark_keeps_its_first_match_whatever_order_the_text_arrives_in() {
        let mut embed = super::Embed::default();
        embed.set_marks(vec![mark("goods", "the goods", None), mark("late", "the goods", Some(3))]);
        // Page 3's text arrives before page 2's; page 1 has no match.
        embed.search_page(2, Some(&page_with("pay for the goods the goods")));
        let found = |embed: &super::Embed| embed.page_marks.iter().map(|m| (m.id.clone(), m.page)).collect::<Vec<_>>();
        assert_eq!(found(&embed), vec![("goods".to_string(), 2), ("late".to_string(), 2)]);
        embed.search_page(1, Some(&page_with("deliver the goods now the goods")));
        embed.search_page(0, Some(&page_with("nothing here")));
        // A later page arriving afterwards never takes the match over.
        embed.search_page(3, Some(&page_with("the goods")));
        assert_eq!(found(&embed), vec![("goods".to_string(), 1), ("late".to_string(), 2)], "one match per mark: the lowest page, host order");
        let first_line_matches = embed.page_marks.iter().filter(|m| m.id == "goods").count();
        assert_eq!(first_line_matches, 1, "only the first of two matches on the page");
        assert_eq!(embed.pages_pending(4).count(), 0, "a found mark asks for no more text");
    }

    #[test]
    fn a_mark_over_the_raw_cap_is_skipped_without_being_folded() {
        let folds_before = text_match::FOLD_CALLS.with(std::cell::Cell::get);
        let mostly_blank = format!("goods{}", " ".repeat(300 * 1024));
        let mut embed = super::Embed::default();
        embed.set_marks(vec![mark("huge", &mostly_blank, None)]);
        assert!(embed.marks.is_empty() && embed.needles.is_empty());
        assert_eq!(text_match::FOLD_CALLS.with(std::cell::Cell::get), folds_before, "never folded");
        assert!(!embed.marks_pending(3), "a skipped mark asks for no text");
    }

    #[test]
    fn a_mark_at_the_raw_cap_that_folds_short_is_kept() {
        let padded = format!("the goods{}", " ".repeat(text_match::MAX_RAW_PASSAGE_BYTES - "the goods".len()));
        let mut embed = super::Embed::default();
        embed.set_marks(vec![mark("padded", &padded, None)]);
        assert_eq!(embed.marks.len(), 1);
        embed.search_page(0, Some(&page_with("pay for the goods")));
        assert_eq!(embed.page_marks.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["padded"]);
    }

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
