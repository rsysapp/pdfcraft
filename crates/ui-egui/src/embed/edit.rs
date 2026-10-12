//! Embed build: edit mode (embed API 0.2). The host page switches the open document to edit mode
//! with an author, a date and permissions. The tools those allow are offered on the rail; comments
//! go to the host; a save hands the host an incremental update of the opened bytes; redactions are
//! only marked, and the host applies them on its server (R6b-49). Everything else stays as in view
//! mode, and every edit is checked here before the engine applies it.

use std::sync::Arc;

use pdfcraft_engine::{Edit, Shape};

use super::protocol::{EditAuthor, EditPermissions, EditorMode, HostEvent, ModeRequest, refusal, unix_seconds};
use super::{Embed, Reading, selection_event};
use crate::comments::CommentTool;
use crate::{Mode, PdfCraftApp, QuickTool};

/// What the host hears for a save in view mode.
pub(crate) const NOTHING_TO_SAVE: &str = "Nothing to save: the document is open for viewing only.";

/// What the host hears when the edited document cannot be written; the engine's own reason goes
/// to the log.
pub(crate) const NOT_WRITTEN: &str = "The document could not be written.";

pub(crate) struct EditSession {
    pub(crate) author: String,
    pub(crate) permissions: EditPermissions,
    /// A comment tool ran: the settled selection goes to the host at the end of the frame.
    comment_asked: bool,
}

/// Who and when the unsaved changes of the open document were made: one author and one date per
/// save (Ruling W3-5). Kept while the document stays open, across mode changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signer {
    id: String,
    name: String,
    date: i64,
}

/// The rail's edit tools, in order.
pub(crate) const RAIL: &[&str] = &[
    "edit.undo",
    "edit.redo",
    "page.rotate",
    "page.rotate_ccw",
    "page.delete",
    "page.organize",
    "page.insert",
    "page.insert_blank",
    "redact.mark",
    "comment.stamp",
    "comment.ink",
    "comment.line",
    "comment.arrow",
    "comment.square",
    "comment.circle",
    "comment.eraser",
];

/// Comment tools: their annotations are discussions, which belong to the host page.
fn is_comment_tool(tool: CommentTool) -> bool {
    matches!(
        tool,
        CommentTool::Note
            | CommentTool::TextBox
            | CommentTool::Highlight
            | CommentTool::Underline
            | CommentTool::StrikeOut
            | CommentTool::Squiggly
            | CommentTool::Callout
            | CommentTool::Caret
            | CommentTool::ReplaceText
            | CommentTool::Attach
    )
}

fn is_drawing_tool(tool: CommentTool) -> bool {
    matches!(
        tool,
        CommentTool::Ink
            | CommentTool::Line
            | CommentTool::Arrow
            | CommentTool::Rectangle
            | CommentTool::Oval
            | CommentTool::Polygon
            | CommentTool::PolyLine
            | CommentTool::Cloud
            | CommentTool::Eraser
    )
}

/// Commands edit mode runs beyond view mode's. Applying redactions is never one (R6b-49), nor is
/// clearing every mark (it would remove the file's own).
pub(crate) fn allows_command(id: &str, permissions: &EditPermissions) -> bool {
    match id {
        "edit.undo" | "edit.redo" => true,
        "page.rotate" | "page.rotate_ccw" | "page.delete" | "page.organize" | "page.insert" | "page.insert_blank" => permissions.organize_pages,
        "form.clear" => permissions.fill_forms,
        "redact.mark" => permissions.redact,
        "comment.stamp" => permissions.stamps,
        _ => CommentTool::from_command(id).is_some_and(|tool| is_drawing_tool(tool) && permissions.drawing),
    }
}

/// Edits edit mode applies. Anything else (page content, links, signatures, protection,
/// flattening, sanitising, applying or clearing redactions, comments) is refused, whatever started
/// it. Changing an existing annotation also needs it to be this session's (`embed_edit_prepare`).
pub(crate) fn allows_edit(edit: &Edit, permissions: &EditPermissions) -> bool {
    match edit {
        Edit::RotatePages { .. } | Edit::DeletePages { .. } | Edit::MovePages { .. } | Edit::InsertBlankPage { .. } | Edit::InsertPagesFrom { .. } => {
            permissions.organize_pages
        }
        Edit::SetFieldValue { .. } | Edit::ResetForm { .. } => permissions.fill_forms,
        Edit::AddCustomStamp { .. } => permissions.stamps,
        Edit::AddAnnotation(annotation) => match &annotation.shape {
            Shape::Redact { .. } => permissions.redact,
            Shape::Stamp { .. } | Shape::CustomStamp { .. } => permissions.stamps,
            Shape::Ink { .. } | Shape::Line { .. } | Shape::Rectangle { .. } | Shape::Oval { .. } | Shape::Polygon { .. } | Shape::PolyLine { .. } => {
                permissions.drawing
            }
            _ => false,
        },
        Edit::EraseInk { .. } => permissions.drawing,
        Edit::MoveAnnotation { .. } | Edit::ResizeAnnotation { .. } | Edit::DeleteAnnotation { .. } => permissions.drawing || permissions.stamps || permissions.redact,
        Edit::Batch { edits, .. } => edits.iter().all(|edit| allows_edit(edit, permissions)),
        _ => false,
    }
}

/// The annotation an edit changes in place, as (page, index in `/Annots`).
fn changed_annotations(edit: &Edit, out: &mut Vec<(usize, usize)>) {
    match edit {
        Edit::EraseInk { page, index, .. } | Edit::MoveAnnotation { page, index, .. } | Edit::ResizeAnnotation { page, index, .. } | Edit::DeleteAnnotation { page, index } => {
            out.push((*page, *index));
        }
        Edit::Batch { edits, .. } => edits.iter().for_each(|edit| changed_annotations(edit, out)),
        _ => {}
    }
}

pub(crate) fn allows_tool(tool: &QuickTool, permissions: &EditPermissions) -> bool {
    match tool {
        QuickTool::Select | QuickTool::Hand => true,
        QuickTool::Comment(tool) => is_drawing_tool(*tool) && permissions.drawing,
        QuickTool::Redact => permissions.redact,
        QuickTool::Stamp(_) | QuickTool::CustomStamp(_) => permissions.stamps,
        _ => false,
    }
}

/// Commands whose refusal is no refused edit: page display, zoom, help and the like (W3-1).
fn is_view_command(id: &str) -> bool {
    ["view.", "help.", "app.", "edit.find", "edit.advanced_search"].iter().any(|prefix| id.starts_with(prefix))
}

/// The marks' rectangles grown until every glyph they reach into lies wholly inside (R6b-54,
/// concern 5): the server's redactor removes any glyph a mark reaches into, and refuses a glyph
/// whose centre lies outside its mark. Glyph boxes are the ones the redactor and the inspector
/// read (`pdfcraft_render::content`). A page whose content cannot be read keeps the marks as drawn.
fn snap_to_glyphs(bytes: &Arc<Vec<u8>>, page: usize, quads: &mut [[f64; 8]]) {
    let Ok(content) = pdfcraft_render::content::ContentDocument::open(bytes.clone(), u64::MAX) else { return };
    let Ok(page_content) = content.content(page) else { return };
    let glyphs = &page_content.glyphs;
    let boxes: Vec<[f64; 4]> = (0..glyphs.len())
        .filter_map(|index| glyphs.glyph(index).map(|(_, rect)| rect.map(f64::from)))
        .filter(|rect| rect.iter().all(|value| value.is_finite()) && rect[0] < rect[2] && rect[1] < rect[3])
        .collect();
    for quad in quads.iter_mut() {
        let xs = [quad[0], quad[2], quad[4], quad[6]];
        let ys = [quad[1], quad[3], quad[5], quad[7]];
        let mut rect = [
            xs.iter().copied().fold(f64::INFINITY, f64::min),
            ys.iter().copied().fold(f64::INFINITY, f64::min),
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ];
        // A few rounds: a glyph taken in can reach a neighbour the drawn mark did not.
        for _ in 0..4 {
            let grown = boxes.iter().filter(|glyph| glyph[0] < rect[2] && glyph[2] > rect[0] && glyph[1] < rect[3] && glyph[3] > rect[1]).fold(rect, |r, g| {
                [r[0].min(g[0]), r[1].min(g[1]), r[2].max(g[2]), r[3].max(g[3])]
            });
            if grown == rect {
                break;
            }
            rect = grown;
        }
        *quad = pdfcraft_engine::rect_quad(rect);
    }
}

impl PdfCraftApp {
    pub fn active_document(&self) -> Option<&pdfcraft_engine::Document> {
        self.active_ids().and_then(|(_, id)| self.session.get(id))
    }

    /// The open document differs from the file as opened (or saved): edit mode's dirty state.
    fn document_differs(&self) -> bool {
        self.active_document().is_some_and(pdfcraft_engine::Document::differs_from_saved)
    }

    /// Run `f` with `embed` back in place, for app code that reads it (`apply_edit`'s gate) while
    /// `embed_logic` holds it.
    fn with_embed<R>(&mut self, embed: &mut Embed, f: impl FnOnce(&mut Self) -> R) -> R {
        self.embed = Some(std::mem::take(embed));
        let result = f(self);
        *embed = self.embed.take().unwrap_or_default();
        result
    }

    pub(crate) fn host_set_mode(&mut self, embed: &mut Embed, request: ModeRequest) {
        let ModeRequest::Edit { author, revision_date, permissions } = request else {
            self.leave_edit(embed);
            embed.bridge.emit(HostEvent::ModeChanged { mode: EditorMode::View });
            return;
        };
        // An encrypted file would be saved encrypted, which the host never takes; a document that
        // did not open leaves nothing to edit.
        let encrypted = self.active_document().is_none_or(|document| document.permissions().is_some() || document.read_only_reason.is_some());
        let (Some(date), true, false) = (unix_seconds(&revision_date), embed.file_open, encrypted) else {
            self.leave_edit(embed);
            embed.bridge.emit(HostEvent::ModeChanged { mode: EditorMode::View });
            return;
        };
        let EditAuthor { id, name } = author;
        let signer = Signer { id, name: name.clone(), date };
        // One author and one date per save: unsaved changes keep theirs until the document is
        // opened again (P6-18, Ruling W3-5). The mode in force stays.
        let dirty = self.document_differs();
        if dirty && embed.signer.as_ref().is_some_and(|made| *made != signer) {
            embed.bridge.emit(HostEvent::EditRefused { code: refusal::INVALID_EDIT_SESSION });
            let mode = if embed.edit.is_some() { EditorMode::Edit } else { EditorMode::View };
            embed.bridge.emit(HostEvent::ModeChanged { mode });
            return;
        }
        if embed.edit.is_none() {
            // A selection the host was told of goes first (embed API 0.2, "Entering edit mode").
            embed.clear_selection_for_move(self.active.and_then(|index| self.views.get_mut(index)));
        }
        self.session.set_view_only(false);
        if embed.signer.as_ref().is_some_and(|made| *made != signer)
            && let Some((_, doc)) = self.active_ids()
        {
            // A new author or date on a clean document: what undo or redo still hold was made by
            // the earlier one, and must not come back under this session.
            self.session.clear_history(doc);
        }
        // What edit mode creates or changes carries the host's date, not the clock (D8).
        self.session.set_edit_date(Some(date));
        self.comment_prefs.author = name.clone();
        embed.signer = Some(signer);
        match embed.edit.as_mut() {
            Some(edit) => {
                edit.author = name;
                edit.permissions = permissions;
            }
            None => embed.edit = Some(EditSession { author: name, permissions, comment_asked: false }),
        }
        embed.bridge.emit(HostEvent::ModeChanged { mode: EditorMode::Edit });
    }

    /// Back to view mode; the edits stay on screen (the host opens the file again to discard them).
    pub(crate) fn leave_edit(&mut self, embed: &mut Embed) {
        if embed.edit.is_some() {
            // What is typed in a field is kept, as the edits are, while edit mode still allows it.
            if let Some((_, doc)) = self.active_ids() {
                self.with_embed(embed, |app| app.commit_typing_in(doc));
            }
            // The selection made while editing is reported only once the person changes it.
            embed.last_selection = self.active.and_then(|index| self.views.get(index)).and_then(crate::DocView::selection_span);
        }
        embed.edit = None;
        self.session.set_view_only(true);
        self.session.set_edit_date(None);
        self.quick_tool = QuickTool::Select;
        for view in &mut self.views {
            view.organize = false;
            view.forms.focus = None;
        }
    }

    /// A save, answered from the document and the mode as they are at the request: a later command
    /// of the same frame (an open, a mode change) must not change what it holds (W3-2, W3-8). Two
    /// requests in one frame for the same document in the same mode answer once.
    pub(crate) fn host_save(&mut self, embed: &mut Embed) {
        let Some((_, doc)) = self.active_ids().filter(|_| embed.edit.is_some()) else {
            let request = (embed.frame, 0, false);
            if embed.last_save != Some(request) {
                embed.last_save = Some(request);
                embed.bridge.emit(HostEvent::SaveFailed { message: NOTHING_TO_SAVE.into() });
            }
            return;
        };
        // What is typed in a field is part of what is saved.
        self.with_embed(embed, |app| app.commit_typing_in(doc));
        let generation = self.session.get(doc).map_or(0, pdfcraft_engine::Document::edit_generation);
        let request = (embed.frame, generation, true);
        if embed.last_save == Some(request) {
            return;
        }
        embed.last_save = Some(request);
        let event = match self.session.save_bytes(doc) {
            Ok(bytes) => HostEvent::SaveRequested { file_name: embed.file_name.clone(), bytes: bytes.as_ref().clone() },
            Err(error) => {
                log::warn!("embed: an edited document could not be written: {error}");
                HostEvent::SaveFailed { message: NOT_WRITTEN.into() }
            }
        };
        embed.bridge.emit(event);
    }

    /// Commands edit mode answers itself; `None` lets `execute` go on.
    pub(crate) fn embed_edit_command(&mut self, id: &str) -> Option<bool> {
        let edit = self.embed.as_mut()?.edit.as_mut()?;
        match id {
            // The editor's own save shortcut: the host decides what a save becomes.
            "file.save" => {
                let mut embed = self.embed.take()?;
                self.host_save(&mut embed);
                self.embed = Some(embed);
                Some(true)
            }
            _ if CommentTool::from_command(id).is_some_and(is_comment_tool) => {
                edit.comment_asked = true;
                Some(true)
            }
            _ => None,
        }
    }

    /// A command `execute` refused: in edit mode, the host hears so once (X19), unless it only
    /// displays or navigates (W3-1).
    pub(crate) fn embed_command_refused(&self, id: &str) {
        let Some(embed) = self.embed.as_ref().filter(|embed| embed.edit.is_some()) else { return };
        if is_view_command(id) {
            return;
        }
        let code = if pdfcraft_engine::commands::command(id).is_some() { refusal::COMMAND_NOT_ALLOWED } else { refusal::UNKNOWN_COMMAND };
        embed.bridge.emit(HostEvent::EditRefused { code });
    }

    /// An edit the engine refused in edit mode (a page that is not there, …).
    pub(crate) fn embed_edit_failed(&self) {
        if let Some(embed) = self.embed.as_ref().filter(|embed| embed.edit.is_some()) {
            embed.bridge.emit(HostEvent::EditRefused { code: refusal::COMMAND_FAILED });
        }
    }

    /// Whether the annotation at (page, index) was made in this edit session: the session's author
    /// and the session's date. Others' markup, and the same person's from an earlier session, are not.
    fn made_in_this_session(&self, page: usize, index: usize, author: &str) -> bool {
        // The page view reads `/M` as shown (to the minute); the author tells sessions apart too.
        let Some(date) = self.session.edit_date_text().map(|date| pdfcraft_render::pretty_date(&date)) else { return false };
        self.active_document().is_some_and(|document| {
            document.info.annotations.iter().any(|annotation| {
                annotation.page == page
                    && annotation.index == index
                    && annotation.author.as_deref() == Some(author)
                    && annotation.modified.as_deref() == Some(date.as_str())
            })
        })
    }

    /// Before `apply_edit`: in edit mode, only what the permissions allow, changes to this
    /// session's annotations only (D24), in the host's author's name, and redaction marks as a
    /// plain black fill over whole glyphs (R6b-11, R6b-54). A refusal is told to the host.
    pub(crate) fn embed_edit_prepare(&mut self, edit: &mut Edit) -> bool {
        let Some(embed) = self.embed.as_ref() else { return true };
        let Some(session) = embed.edit.as_ref() else { return true };
        let author = session.author.clone();
        let mut changed = Vec::new();
        changed_annotations(edit, &mut changed);
        let allowed = allows_edit(edit, &session.permissions) && changed.iter().all(|(page, index)| self.made_in_this_session(*page, *index, &author));
        if !allowed {
            embed.bridge.emit(HostEvent::EditRefused { code: refusal::COMMAND_NOT_ALLOWED });
            return false;
        }
        let bytes = self.active_document().map(|document| document.bytes.clone());
        prepare_annotations(edit, &author, bytes.as_ref());
        true
    }

    /// Inserting pages from a file is the one file picker edit mode shows.
    pub(crate) fn embed_allows_picker(&self) -> bool {
        self.embed.as_ref().and_then(|embed| embed.edit.as_ref()).is_some_and(|edit| edit.permissions.organize_pages)
    }

    /// Edit mode's profile, whatever ran this frame: the rail's tools and the stamp panel only.
    pub(crate) fn enforce_edit_profile(&mut self, permissions: EditPermissions) {
        self.mode = Mode::Read;
        if !(permissions.stamps && matches!(self.left, crate::LeftPanel::Tool("stamp"))) {
            self.left_open = false;
        }
        self.right = None;
        self.dialog = None;
        self.palette_open = false;
        self.close_request = None;
        self.password_prompt = None;
        self.pending_link = None;
        self.full_screen = false;
        // The redaction tool has no fill choice and no overlay text (R6b-11, P6-33).
        self.redact_prefs = crate::redact_ui::RedactPrefs::default();
        if !allows_tool(&self.quick_tool, &permissions) {
            self.quick_tool = QuickTool::Select;
        }
        if !permissions.organize_pages {
            for view in &mut self.views {
                view.organize = false;
            }
        }
        if !permissions.fill_forms {
            for view in &mut self.views {
                view.forms.focus = None;
            }
        }
    }
}

/// New annotations in the host's author's name; a redaction mark a plain black fill with no
/// overlay, grown over the glyphs it reaches into.
fn prepare_annotations(edit: &mut Edit, author: &str, bytes: Option<&Arc<Vec<u8>>>) {
    match edit {
        Edit::AddAnnotation(annotation) => {
            annotation.author = author.to_string();
            if let Shape::Redact { quads, overlay, look } = &mut annotation.shape {
                overlay.clear();
                *look = Default::default();
                annotation.style.fill = Some([0.0, 0.0, 0.0]);
                annotation.contents.clear();
                if let Some(bytes) = bytes {
                    snap_to_glyphs(bytes, annotation.page, quads);
                }
            }
        }
        Edit::AddCustomStamp { author: stamp_author, .. } => *stamp_author = author.to_string(),
        Edit::Batch { edits, .. } => edits.iter_mut().for_each(|edit| prepare_annotations(edit, author, bytes)),
        _ => {}
    }
}

/// After the frame, in edit mode: a comment asked for, the dirty state and the number of marked
/// areas. Selection events are not sent in edit mode.
pub(crate) fn frame_events(app: &mut PdfCraftApp, ctx: &egui::Context) {
    let dirty = app.document_differs();
    let marks = app.active_document().map_or(0, pdfcraft_engine::Document::redaction_marks);
    let Some(embed) = app.embed.as_mut() else { return };
    let Some(view) = app.active.and_then(|index| app.views.get(index)) else { return };
    // Only marks this editing added count: the file's own unapplied marks stay as they are (R6b-55).
    let marks = marks.saturating_sub(embed.base_marks);
    if marks != embed.reported_marks {
        embed.reported_marks = marks;
        embed.bridge.emit(HostEvent::RedactionsPending { marks });
    }
    if embed.reported_dirty != dirty {
        embed.reported_dirty = dirty;
        embed.bridge.emit(HostEvent::DirtyChanged { dirty });
    }
    let Some(edit) = embed.edit.as_mut() else { return };
    if edit.comment_asked {
        match view.selection_span().map(|(page, range)| selection_event(view, page, range, ctx.zoom_factor())) {
            Some(Reading::Text(HostEvent::Selection { text, copy_text, page, x, y })) => {
                edit.comment_asked = false;
                embed.bridge.emit(HostEvent::CommentRequested { text, copy_text, page, x, y });
            }
            // Off screen or its text not in yet: tried again next frame.
            Some(Reading::NotReady) => {}
            _ => edit.comment_asked = false,
        }
    }
}
