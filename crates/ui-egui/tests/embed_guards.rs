//! View mode refuses every edit path by a guard, not only by hiding it; Copy still works.

mod embed_support;

use egui::{Event, Key, Modifiers};
use embed_support::{embedded, open, pages};
use pdfcraft_engine::{Edit, NewAnnotation, Shape, Style};
use pdfcraft_ui_egui::{FilePurpose, QuickTool};

fn square() -> Edit {
    Edit::AddAnnotation(NewAnnotation {
        page: 0,
        shape: Shape::Rectangle { rect: [50.0, 50.0, 120.0, 120.0] },
        style: Style { color: [1.0, 0.0, 0.0], opacity: 1.0, width: 2.0, fill: None },
        contents: "x".into(),
        author: "Reviewer".into(),
    })
}

#[test]
fn every_edit_path_is_refused_in_view_mode() {
    let (mut h, bridge) = embedded();
    let original = pages(&[&["The quick brown fox"], &["jumps over the dog"]]);
    open(&mut h, &bridge, "view.pdf", original.clone());
    let id = h.state().views[0].id;
    // The edit chokepoint, undo and redo.
    assert!(!h.state_mut().apply_edit(square()));
    h.state_mut().undo();
    h.state_mut().redo();
    // Command dispatch: tools, saving, printing, exporting, page changes, OCR, signing, files, help.
    for command in [
        "comment.highlight",
        "comment.note",
        "edit.text",
        "file.save",
        "file.save_as",
        "print.dialog",
        "export.image",
        "page.delete",
        "page.rotate",
        "ocr.recognize",
        "comment.hide_all",
        "sign.digital",
        "page.combine",
        "help.discord",
        "help.about",
        "file.open",
        "view.full_screen",
    ] {
        assert!(!h.state_mut().execute(command), "{command} is refused");
    }
    // Files for combining, inserting or OCR, and a file dropped on the page.
    h.state_mut().use_files(FilePurpose::InsertPages, vec![("more.pdf".into(), original.clone())]);
    h.state_mut().use_files(FilePurpose::Combine, vec![("more.pdf".into(), original.clone())]);
    assert!(h.state_mut().open_bytes("dropped.pdf", None, original.clone()).is_err());
    // Keyboard: undo, save, print, delete.
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.key_press_modifiers(Modifiers::COMMAND, Key::S);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.key_press(Key::Delete);
    h.run_steps(4);
    let state = h.state();
    assert_eq!(state.views.len(), 1);
    let doc = state.session.get(id).expect("still open");
    assert!(!doc.dirty);
    assert_eq!(doc.info.pages.len(), 2);
    assert!(doc.info.annotations.is_empty());
    assert_eq!(*doc.bytes, original);
    assert!(state.dialog.is_none() && !state.palette_open && state.combine_draft.is_empty());
    assert!(matches!(state.quick_tool, QuickTool::Select | QuickTool::Hand));
}

#[test]
fn find_and_page_display_commands_still_run() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "view.pdf", pages(&[&["The quick brown fox"]]));
    assert!(h.state_mut().execute("edit.find"));
    assert!(h.state_mut().execute("view.layout.single"));
    h.run_steps(2);
    assert!(h.state().views[0].find.is_some());
}

#[test]
fn copy_still_copies_the_selection() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "copy.pdf", pages(&[&["The quick brown fox"]]));
    // Glyphs: "The" 0–2, the space 3, "quick" 4–8.
    h.state_mut().views[0].select_text(0, 4, 8);
    h.run_steps(1);
    h.event(Event::Copy);
    let mut copied = false;
    for _ in 0..4 {
        h.step();
        copied |= h.output().platform_output.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(t) if t == "quick"));
    }
    assert!(copied, "⌘C copies the selection");
}

// Every way the app can run a command, open bytes, save, print, download or follow a link,
// driven in an embedded app. Each test watches every frame, because view mode also closes
// dialogs at the end of a frame: a leak would show between frames, not after them.

use egui_kittest::Harness;
use embed_support::write_pdf;
use pdfcraft_engine::commands::COMMANDS;
use pdfcraft_engine::js::{JsOutput, Request};
use pdfcraft_ui_egui::embed::{HostBridge, HostEvent};
use pdfcraft_ui_egui::{Dialog, LinkOrigin, OsEvent, PdfCraftApp, SaveTarget};

/// The commands view mode runs (the embed module's own list, as the contract states it).
const ALLOWED: &[&str] = &[
    "edit.find",
    "view.continuous",
    "view.layout.single",
    "view.layout.two_up",
    "view.layout.cover",
    "view.fit_width_scrolling",
    "view.fit_one_page",
    "view.fit_visible",
];

/// What escaped view mode in the frame just run, if anything: an address opened, full screen
/// entered, a dialog or the palette showing, or a URL recorded as opened.
fn leak(h: &Harness<'static, PdfCraftApp>) -> Option<String> {
    let out = h.output();
    if let Some(url) = out.platform_output.commands.iter().find_map(|c| match c {
        egui::OutputCommand::OpenUrl(u) => Some(u.url.clone()),
        _ => None,
    }) {
        return Some(format!("opened {url}"));
    }
    if out.viewport_output.values().any(|v| v.commands.iter().any(|c| matches!(c, egui::ViewportCommand::Fullscreen(true)))) {
        return Some("entered full screen".into());
    }
    let state = h.state();
    if let Some(dialog) = state.dialog {
        return Some(format!("showed {dialog:?}"));
    }
    if state.palette_open {
        return Some("showed the palette".into());
    }
    state.last_opened_url.as_ref().map(|url| format!("recorded {url} as opened"))
}

/// Run `frames` frames, failing on the first leak.
fn run_watching(h: &mut Harness<'static, PdfCraftApp>, frames: usize, what: &str) {
    for _ in 0..frames {
        h.step();
        if let Some(l) = leak(h) {
            panic!("{what}: {l}");
        }
    }
}

/// The document is still the host's: one view, unchanged bytes, not dirty, no annotations.
fn assert_untouched(h: &Harness<'static, PdfCraftApp>, original: &[u8]) {
    let state = h.state();
    assert_eq!(state.views.len(), 1, "no other document opened");
    let doc = state.session.get(state.views[0].id).expect("still open");
    assert!(!doc.dirty, "not dirty");
    assert!(doc.info.annotations.is_empty(), "no annotations");
    assert_eq!(*doc.bytes, *original, "bytes unchanged");
}

/// A temporary path that must stay unwritten.
fn temp_path(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("pdfcraft-embed-guard-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn every_registered_command_outside_view_mode_is_refused() {
    let (mut h, bridge) = embedded();
    let original = pages(&[&["The quick brown fox"], &["jumps over the dog"]]);
    open(&mut h, &bridge, "view.pdf", original.clone());
    // Menus, the palette, the tool panels, `run_command` and automation all dispatch through here.
    for spec in COMMANDS {
        let ran = h.state_mut().execute(spec.id);
        if !ALLOWED.contains(&spec.id) {
            assert!(!ran, "{} is refused", spec.id);
        }
        if let Some(l) = leak(&h) {
            panic!("{}: {l}", spec.id);
        }
    }
    run_watching(&mut h, 4, "after every command");
    assert_untouched(&h, &original);
    assert!(!h.state().full_screen && h.state().combine_draft.is_empty());
}

#[test]
fn every_registered_shortcut_is_refused_or_only_changes_the_view() {
    let (mut h, bridge) = embedded();
    let original = pages(&[&["The quick brown fox"], &["jumps over the dog"]]);
    open(&mut h, &bridge, "view.pdf", original.clone());
    let mut pressed = 0;
    let registered = COMMANDS.iter().filter(|c| c.shortcut.is_some()).count();
    for spec in COMMANDS {
        let Some(s) = spec.shortcut else { continue };
        let Some(key) = Key::from_name(s.key) else { continue };
        let mut modifiers = Modifiers::NONE;
        if s.command {
            modifiers |= Modifiers::COMMAND;
        }
        if s.shift {
            modifiers |= Modifiers::SHIFT;
        }
        if s.mac_ctrl {
            modifiers |= Modifiers::CTRL;
        }
        h.key_press_modifiers(modifiers, key);
        run_watching(&mut h, 3, spec.id);
        pressed += 1;
    }
    assert_eq!(pressed, registered, "every registered shortcut was pressed");
    assert_untouched(&h, &original);
    assert!(!h.state().full_screen);
}

/// Two pages; page 1 has push buttons for the Named actions Print, SaveAs and NextPage, a URI
/// action and a script that prints.
fn buttons() -> Vec<u8> {
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [5 0 R 6 0 R 7 0 R 8 0 R 9 0 R] /DA (/Helv 0 Tf 0 g) >> >>".into(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 400] >>".into(),
        "<< /Type /Page /Parent 2 0 R /Annots [5 0 R 6 0 R 7 0 R 8 0 R 9 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R >>".into(),
        "<< /Type /Annot /Subtype /Widget /FT /Btn /Ff 65536 /T (print) /Rect [20 300 100 320] /P 3 0 R /A << /S /Named /N /Print >> >>".into(),
        "<< /Type /Annot /Subtype /Widget /FT /Btn /Ff 65536 /T (saveas) /Rect [120 300 200 320] /P 3 0 R /A << /S /Named /N /SaveAs >> >>".into(),
        "<< /Type /Annot /Subtype /Widget /FT /Btn /Ff 65536 /T (web) /Rect [20 260 100 280] /P 3 0 R /A << /S /URI /URI (https://example.com/) >> >>".into(),
        "<< /Type /Annot /Subtype /Widget /FT /Btn /Ff 65536 /T (script) /Rect [120 260 200 280] /P 3 0 R /AA << /U << /S /JavaScript /JS (this.print\\({bUI: true}\\);) >> >> >>".into(),
        "<< /Type /Annot /Subtype /Widget /FT /Btn /Ff 65536 /T (next) /Rect [20 220 100 240] /P 3 0 R /A << /S /Named /N /NextPage >> >>".into(),
    ])
}

/// Click push button `name`, failing on any frame that leaks when `watch` is set.
fn click_button(h: &mut Harness<'static, PdfCraftApp>, name: &str, watch: bool) {
    let at = {
        let s = h.state();
        let doc = s.session.get(s.views[0].id).expect("open");
        let field = doc.form.iter().find(|f| f.name == name).expect("the button");
        pdfcraft_ui_egui::forms_ui::field_screen_rect(&s.views[0], &doc.info, field, 0).expect("on screen").center()
    };
    h.hover_at(at);
    h.step();
    h.drag_at(at);
    h.step();
    h.drop_at(at);
    if watch {
        run_watching(h, 4, name);
    } else {
        h.run_steps(4);
    }
}

#[test]
fn named_print_and_save_as_buttons_links_and_scripts_are_refused() {
    // The same click in the full app opens the Print dialog, so its absence below means something.
    let mut full = Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_eframe(|_cc| {
        let mut app = PdfCraftApp::new();
        app.open_bytes("buttons.pdf", None, buttons()).expect("opens");
        app
    });
    full.run_steps(6);
    click_button(&mut full, "print", false);
    assert_eq!(full.state().dialog, Some(Dialog::Print), "the Named Print action opens Print in the full app");

    let (mut h, bridge) = embedded();
    let original = buttons();
    open(&mut h, &bridge, "buttons.pdf", original.clone());
    // A Save As that got through would write here without asking.
    let target = temp_path("saveas.pdf");
    h.state_mut().pick_override = Some(vec![target.to_string_lossy().into_owned()]);
    h.state_mut().save_override = Some(target.to_string_lossy().into_owned());
    for name in ["print", "saveas", "web", "script", "next"] {
        h.state_mut().toast = None;
        click_button(&mut h, name, true);
        // The click lands on the button; a view-only document allows no form use, so the button
        // only says so and runs no action (not even NextPage).
        let notice = h.state().toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
        assert!(notice.contains("don't allow filling in form fields"), "{name}: {notice:?}");
    }
    assert_eq!(h.state().views[0].current, 0);
    assert!(!target.exists(), "nothing was saved");
    assert_untouched(&h, &original);
}

#[test]
fn script_requests_and_document_links_are_refused() {
    let (mut h, bridge) = embedded();
    let original = pages(&[&["The quick brown fox"]]);
    open(&mut h, &bridge, "view.pdf", original.clone());
    let id = h.state().views[0].id;
    let target = temp_path("script.pdf");
    h.state_mut().pick_override = Some(vec![target.to_string_lossy().into_owned()]);
    h.state_mut().save_override = Some(target.to_string_lossy().into_owned());
    let requests = vec![Request::Print, Request::SaveAs, Request::LaunchUrl("https://example.com/".into())];
    h.state_mut().handle_js(id, JsOutput { requests, ..Default::default() });
    assert!(h.state().dialog.is_none(), "a script's print opens no Print dialog");
    // A link on the page, then the user's "allow": still nothing opens.
    h.state_mut().request_document_url("https://example.com/", LinkOrigin::Link);
    h.state_mut().resolve_pending_link(true);
    h.state_mut().open_url("https://example.com/");
    run_watching(&mut h, 4, "script requests and links");
    assert!(!target.exists(), "nothing was saved");
    assert_untouched(&h, &original);
}

#[test]
fn help_and_update_links_are_refused() {
    let (mut h, bridge) = embedded();
    open(&mut h, &bridge, "view.pdf", pages(&[&["The quick brown fox"]]));
    for command in COMMANDS.iter().map(|c| c.id).filter(|id| id.starts_with("help.")) {
        assert!(!h.state_mut().execute(command), "{command} is refused");
    }
    // Without an update source, checking falls back to opening the releases page.
    h.state_mut().check_for_updates();
    run_watching(&mut h, 3, "help and updates");
}

/// A file dropped on the window, as the windowing layer hands it over.
#[derive(Debug)]
struct Dropped {
    path: std::path::PathBuf,
    bytes: Vec<u8>,
}

impl egui::DroppedFile for Dropped {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        Ok(self.bytes.clone())
    }
}

/// No `opened` (or anything else) for the host after files arrived from outside it.
fn assert_no_host_events(bridge: &HostBridge) {
    let events = bridge.take_events();
    assert!(!events.iter().any(|e| matches!(e, HostEvent::Opened { .. })), "{events:?}");
}

#[test]
fn dropped_picked_and_os_files_never_open() {
    let (mut h, bridge) = embedded();
    let original = pages(&[&["The quick brown fox"]]);
    open(&mut h, &bridge, "view.pdf", original.clone());
    let other = pages(&[&["Another"], &["file"]]);
    let on_disk = temp_path("on-disk.pdf");
    std::fs::write(&on_disk, &other).expect("writes the fixture");
    // A drop with bytes (the web, and some desktops) and one with a path (most desktops).
    let in_memory: egui::DroppedFileHandle = std::sync::Arc::new(Dropped { path: "dropped.pdf".into(), bytes: other.clone() });
    let by_path: egui::DroppedFileHandle = std::sync::Arc::new(Dropped { path: on_disk.clone(), bytes: Vec::new() });
    h.input_mut().dropped_files.extend([in_memory, by_path]);
    run_watching(&mut h, 3, "dropped files");
    // The web's drop and file-picker queue.
    h.state().inbox.lock().expect("inbox").push(("picked.pdf".into(), other.clone()));
    run_watching(&mut h, 3, "the inbox");
    // Files the operating system hands over (Finder, Open With).
    let path = on_disk.to_string_lossy().into_owned();
    let mut once = Some(OsEvent::Open(vec![path.clone()]));
    h.state_mut().os_events = Some(Box::new(move || once.take().into_iter().collect()));
    run_watching(&mut h, 3, "files from the operating system");
    // Recent files and direct calls.
    h.state_mut().open_recent(&path);
    h.state_mut().open_path(&path);
    run_watching(&mut h, 3, "recent files");
    let _ = std::fs::remove_file(&on_disk);
    assert_no_host_events(&bridge);
    assert_untouched(&h, &original);
    assert!(h.state().combine_draft.is_empty());
}

#[test]
fn saving_and_printing_are_refused_even_with_a_destination_set() {
    let (mut h, bridge) = embedded();
    let original = pages(&[&["The quick brown fox"]]);
    open(&mut h, &bridge, "view.pdf", original.clone());
    let target = temp_path("saved.pdf");
    h.state_mut().pick_override = Some(vec![target.to_string_lossy().into_owned()]);
    h.state_mut().save_override = Some(target.to_string_lossy().into_owned());
    assert!(!h.state_mut().save_active(SaveTarget::As));
    assert!(!h.state_mut().save_active(SaveTarget::InPlace));
    assert!(!h.state_mut().save_view(0, SaveTarget::As));
    h.state_mut().open_print();
    assert!(h.state().dialog.is_none(), "no Print dialog");
    // Print ▸ Save as PDF: no printer, so the print-ready file would be written to the override.
    h.state_mut().print_draft.printer = None;
    assert!(!h.state_mut().print_now());
    run_watching(&mut h, 4, "saving and printing");
    assert!(!target.exists(), "nothing was written");
    assert_untouched(&h, &original);
}

/// A one-page file with another PDF attached, `inner.pdf`.
fn with_attachment() -> Vec<u8> {
    let inner = String::from_utf8(pages(&[&["Inner"], &["file"]])).expect("the fixture writer writes ASCII");
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles << /Names [(inner.pdf) 4 0 R] >> >> >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 400] >>".into(),
        "<< /Type /Page /Parent 2 0 R >>".into(),
        "<< /Type /Filespec /F (inner.pdf) /UF (inner.pdf) /EF << /F 5 0 R >> >>".into(),
        format!("<< /Type /EmbeddedFile /Length {} >>\nstream\n{inner}\nendstream", inner.len()),
    ])
}

#[test]
fn attachments_neither_open_nor_save() {
    let (mut h, bridge) = embedded();
    let original = with_attachment();
    open(&mut h, &bridge, "attached.pdf", original.clone());
    let id = h.state().views[0].id;
    assert_eq!(h.state().session.get(id).map(|d| d.info.attachments.len()), Some(1), "the fixture has its attachment");
    let target = temp_path("inner.pdf");
    h.state_mut().pick_override = Some(vec![target.to_string_lossy().into_owned()]);
    h.state_mut().attachment_action(id, 0, true);
    h.state_mut().attachment_action(id, 0, false);
    run_watching(&mut h, 4, "attachments");
    assert!(!target.exists(), "the attachment was not saved");
    assert_no_host_events(&bridge);
    assert_untouched(&h, &original);
}

#[test]
fn the_control_channel_runs_no_refused_command_and_opens_nothing() {
    use std::sync::{Arc, Mutex};
    let slot: Arc<Mutex<Option<pdfcraft_ui_egui::control::ControlClient>>> = Arc::default();
    let s = slot.clone();
    let bridge = HostBridge::default();
    let app_bridge = bridge.clone();
    let mut h = Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_eframe(move |cc| {
        let mut app = PdfCraftApp::embedded(app_bridge);
        *s.lock().expect("slot") = Some(app.attach_control(&cc.egui_ctx));
        app
    });
    let original = pages(&[&["The quick brown fox"]]);
    open(&mut h, &bridge, "view.pdf", original.clone());
    let client = slot.lock().expect("slot").take().expect("attached");
    let path = temp_path("control.pdf");
    std::fs::write(&path, pages(&[&["Other"]])).expect("writes the fixture");
    let mut call = |method: &str, params: serde_json::Value| {
        let rx = client.send(method, params);
        for _ in 0..30 {
            h.step();
            if let Some(l) = leak(&h) {
                panic!("{method}: {l}");
            }
            if let Ok(reply) = rx.try_recv() {
                return reply;
            }
        }
        panic!("{method}: no reply");
    };
    // `ui.command` answers once dispatch returns; what counts is that nothing ran (checked below).
    for id in ["file.save", "file.save_as", "print.dialog", "help.website", "view.full_screen", "file.close", "page.delete"] {
        let _ = call("ui.command", serde_json::json!({ "id": id }));
    }
    assert!(call("ui.open", serde_json::json!({ "path": path.to_string_lossy() })).is_err(), "ui.open is refused");
    let _ = std::fs::remove_file(&path);
    assert_no_host_events(&bridge);
    assert_untouched(&h, &original);
}

#[test]
fn the_embedded_session_never_writes_the_document_information() {
    // R6b-45: the host refuses any change to /Info and records the edit time itself, so the
    // embed build's session stamps no /ModDate on any save; the full app still does.
    let (h, _bridge) = embedded();
    assert!(h.state().session.keeps_document_information());
    assert!(!pdfcraft_ui_egui::PdfCraftApp::new().session.keeps_document_information());
}

#[test]
fn the_embedded_session_never_applies_redactions() {
    // R6b-49: the embed build marks redactions; only the host's server applies them.
    let (h, _bridge) = embedded();
    assert!(h.state().session.redaction_marks_only());
    assert!(!pdfcraft_ui_egui::PdfCraftApp::new().session.redaction_marks_only());
}
