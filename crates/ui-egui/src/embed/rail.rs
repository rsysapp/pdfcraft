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
            chrome::page_cluster(app, ui, index, page_count, &labels, true);
        });
}
