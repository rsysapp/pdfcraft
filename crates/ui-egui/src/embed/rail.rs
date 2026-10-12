//! The embed build's navigation rail: find at the top and the app's own page cluster at the
//! bottom (`chrome::page_cluster`, shared with the right rail), without the panel buttons. Its
//! labels are the app's own, so every interface language already has them.

use egui::Stroke;

use crate::theme::Tokens;
use crate::{PdfCraftApp, chrome, icons};

pub(crate) fn show(app: &mut PdfCraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let Some((index, id)) = app.active_ids() else { return };
    let Some(doc) = app.session.get(id) else { return };
    let page_count = doc.info.pages.len();
    let labels: Vec<String> = doc.info.pages.iter().map(|p| p.label.clone()).collect();
    egui::Panel::right("embed-rail")
        .resizable(false)
        .exact_size(48.0)
        .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin::symmetric(6, 8)).stroke(Stroke::new(1.0, t.divider)))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            if let Some(view) = app.views.get_mut(index)
                && icons::button(ui, "search", 34.0, view.find.is_some(), tl!("Find text")).clicked()
            {
                if view.find.is_some() {
                    view.find = None;
                } else {
                    view.open_find();
                }
            }
            edit_tools(app, ui);
            chrome::page_cluster(app, ui, index, page_count, &labels, true);
        });
}

/// Embed API 0.2: edit mode's tools, below find, as the permissions allow (`embed::edit`). The
/// labels are the commands' own, so every interface language already has them.
fn edit_tools(app: &mut PdfCraftApp, ui: &mut egui::Ui) {
    let Some(permissions) = app.embed.as_ref().and_then(|embed| embed.edit.as_ref()).map(|edit| edit.permissions) else { return };
    ui.separator();
    let mut chosen = None;
    for id in super::edit::RAIL.iter().filter(|id| super::edit::allows_command(id, &permissions)) {
        let Some(spec) = pdfcraft_engine::commands::command(id) else { continue };
        if icons::button(ui, spec.icon, 34.0, false, tl!(spec.label)).clicked() {
            chosen = Some(*id);
        }
    }
    if let Some(id) = chosen {
        app.execute(id);
    }
}
