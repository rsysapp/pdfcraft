//! Shared by the embed build's tests: fixtures and a harness around `PdfCraftApp::embedded`.
#![allow(dead_code)] // Each test file uses part of it.

use egui_kittest::Harness;
use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::embed::{HostBridge, HostCommand, HostEvent};

/// Numbered objects as a PDF with a correct cross-reference table.
pub fn write_pdf(objs: &[String]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

/// One 400×300 page per entry; each shows its lines from the top, 24 points apart, in Helvetica
/// 14 with WinAnsi encoding (so `\223` and `\224` are curly double quotes).
pub fn pages(pages: &[&[&str]]) -> Vec<u8> {
    let mut objs: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".into()];
    let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    objs.push(format!("<< /Type /Pages /Kids [{}] /Count {} /MediaBox [0 0 400 300] >>", kids.join(" "), pages.len()));
    objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into());
    for (i, lines) in pages.iter().enumerate() {
        objs.push(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>", 5 + 2 * i));
        let shown: Vec<String> = lines.iter().map(|l| format!("({l}) Tj 0 -24 Td")).collect();
        let body = format!("BT /F1 14 Tf 40 260 Td {} ET", shown.join(" "));
        objs.push(format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()));
    }
    write_pdf(&objs)
}

/// A one-page file that needs `user` to open (AES-256).
pub fn protected(user: &str) -> Vec<u8> {
    let mut doc = pdfcraft_cos::Document::open(std::sync::Arc::new(pages(&[&["Secret"]]))).expect("fixture parses");
    doc.set_encryption(&pdfcraft_cos::NewEncryption {
        algorithm: pdfcraft_cos::Algorithm::Aes256,
        user_password: user,
        owner_password: "owner",
        permissions: -1,
        encrypt_metadata: true,
        seed: [7; 32],
    })
    .expect("encrypts");
    pdfcraft_cos::write_full(&doc, &Default::default()).expect("writes")
}

/// The embedded app and the host's end of its bridge, its context set up as the embed build's is.
pub fn embedded() -> (Harness<'static, PdfCraftApp>, HostBridge) {
    embedded_with(true)
}

/// [`embedded`], with or without the embed build's context setup (`embed::prepare_context`).
pub fn embedded_with(prepared: bool) -> (Harness<'static, PdfCraftApp>, HostBridge) {
    let bridge = HostBridge::default();
    let app_bridge = bridge.clone();
    // Shortcut tooltips name the platform's keys ("Zoom in (⌘+)"): one platform for every test
    // machine (kittest reports Nix unless told otherwise).
    let h = Harness::builder().with_size(egui::vec2(900.0, 700.0)).with_step_dt(1.0 / 60.0).with_os(egui::os::OperatingSystem::Mac).build_eframe(
        move |cc| {
            if prepared {
                pdfcraft_ui_egui::embed::prepare_context(&cc.egui_ctx);
            }
            PdfCraftApp::embedded(app_bridge)
        },
    );
    (h, bridge)
}

/// Run frames until page renders, text layers and host work (jumps, marks) are done.
pub fn settle(h: &mut Harness<'static, PdfCraftApp>) {
    for _ in 0..500 {
        h.run_steps(2);
        if !h.state().render_pending() && !h.state().host_work_pending() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the page view did not settle");
}

/// Open `bytes` as the host does and return the events that followed.
pub fn open(h: &mut Harness<'static, PdfCraftApp>, bridge: &HostBridge, name: &str, bytes: Vec<u8>) -> Vec<HostEvent> {
    bridge.take_events();
    bridge.send(HostCommand::Open { name: name.into(), bytes });
    h.run_steps(3);
    settle(h);
    bridge.take_events()
}

/// Three pages of an agreement-like text; page 1 quotes "Goods" with curly quotes.
pub fn three_pages() -> Vec<u8> {
    pages(&[
        &["Definitions", "The \\223Goods\\224 are the packaging", "described in Schedule 1."],
        &["Delivery", "The Supplier delivers the Goods", "within five days."],
        &["Payment", "The Buyer pays for the Goods", "within thirty days."],
    ])
}
