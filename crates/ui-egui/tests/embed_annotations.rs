//! Go-live gate 2: comments without an appearance stream are drawn in view mode, from the
//! display-only copy, and the file stays as it was.

mod embed_support;

use std::sync::Arc;

use embed_support::{embedded, open, write_pdf};
use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RenderedPage};
use pdfcraft_ui_egui::embed::HostEvent;

/// One 200×300 page: a yellow highlight, a red strike-out, a blue note, an orange text box, and a
/// highlight without a colour, none with /AP.
fn comments_without_appearances() -> Vec<u8> {
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".into(),
        "<< /Type /Page /Parent 2 0 R /Annots [4 0 R 5 0 R 6 0 R 7 0 R 8 0 R] >>".into(),
        "<< /Type /Annot /Subtype /Highlight /Rect [20 240 120 260] /QuadPoints [20 260 120 260 20 240 120 240] /C [1 1 0] >>".into(),
        "<< /Type /Annot /Subtype /StrikeOut /Rect [20 200 120 220] /QuadPoints [20 220 120 220 20 200 120 200] /C [1 0 0] >>".into(),
        "<< /Type /Annot /Subtype /Text /Rect [20 140 44 164] /Contents (Note) /C [0 0 1] >>".into(),
        "<< /Type /Annot /Subtype /FreeText /Rect [20 40 180 100] /Contents (Box) /DA (/Helv 10 Tf 0 0 1 rg) /Border [0 0 1] /C [.85 .47 .02] >>"
            .into(),
        "<< /Type /Annot /Subtype /Highlight /Rect [130 240 190 260] /QuadPoints [130 260 190 260 130 240 190 240] >>".into(),
    ])
}

fn render(bytes: &Arc<Vec<u8>>) -> RenderedPage {
    let page = PageRenderer::new(bytes.clone(), RenderConfig::default()).render(RenderRequest { page: 0, scale: 1.0, ..Default::default() });
    assert!(page.error.is_none(), "{:?}", page.error);
    page
}

/// RGB at (x, y), y down from the top of the page.
fn px(page: &RenderedPage, x: u32, y: u32) -> [u8; 3] {
    let at = ((y * page.width + x) * 4) as usize;
    [page.rgba[at], page.rgba[at + 1], page.rgba[at + 2]]
}

fn any(page: &RenderedPage, (x0, y0, x1, y1): (u32, u32, u32, u32), want: impl Fn([u8; 3]) -> bool) -> bool {
    (y0..y1).any(|y| (x0..x1).any(|x| want(px(page, x, y))))
}

#[test]
fn comments_without_appearance_streams_are_drawn_in_view_mode() {
    let (mut h, bridge) = embedded();
    let original = comments_without_appearances();
    let events = open(&mut h, &bridge, "comments.pdf", original.clone());
    assert_eq!(events.first(), Some(&HostEvent::Opened { pages: 1 }));
    let id = h.state().views[0].id;
    let doc = h.state().session.get(id).expect("open");
    assert!(!doc.dirty);
    assert_eq!(*doc.bytes, original, "the file is unchanged");
    assert!(!Arc::ptr_eq(&doc.display, &doc.bytes), "appearances were drawn for display");
    let shown = render(&doc.display);
    assert_eq!(px(&shown, 70, 50), [255, 255, 0], "the highlight");
    assert!(any(&shown, (20, 80, 120, 100), |c| c[0] > 200 && c[1] < 80 && c[2] < 80), "the strike-out line");
    assert!(any(&shown, (20, 136, 44, 160), |c| c != [255, 255, 255]), "the note icon");
    assert_eq!(px(&shown, 100, 250), [217, 120, 5], "the text box background");
    assert!(!any(&shown, (130, 40, 190, 60), |c| c != [255, 255, 255]), "a highlight without a colour draws nothing");
    // The original alone leaves the text box white: the display copy is what draws it.
    assert_eq!(px(&render(&doc.bytes), 100, 250), [255, 255, 255]);
}
