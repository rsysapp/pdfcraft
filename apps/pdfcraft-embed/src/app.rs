//! The app the runner drives: PdfCraft in view mode, plus handing its events to the host.

use std::cell::RefCell;
use std::rc::Rc;

use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::embed::{HostBridge, HostEvent};
use wasm_bindgen::JsValue;

/// The host's listener, shared with `EmbeddedEditor`.
pub(crate) type Listener = Rc<RefCell<Option<js_sys::Function>>>;

pub(crate) struct EmbedApp {
    pub(crate) inner: PdfCraftApp,
    pub(crate) bridge: HostBridge,
    pub(crate) listener: Listener,
    /// Resolves `mount`'s promise once the first frame ran.
    pub(crate) first_frame: Option<js_sys::Function>,
}

impl eframe::App for EmbedApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.inner.raw_input_hook(ctx, raw);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.inner.logic(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.inner.ui(ui, frame);
        if let Some(resolve) = self.first_frame.take()
            && let Err(e) = resolve.call0(&JsValue::NULL)
        {
            web_sys::console::error_1(&e);
        }
        let events = self.bridge.take_events();
        if !events.is_empty() {
            // A task, not a call: the listener runs after this frame is painted and returned.
            wasm_bindgen_futures::spawn_local(deliver(self.listener.clone(), events));
        }
    }

    // Nothing is read from or written to the host page's storage: no settings, no egui memory.
    // (`save` keeps the trait's default, which stores nothing.)
    fn persist_egui_memory(&self) -> bool {
        false
    }
}

async fn deliver(listener: Listener, events: Vec<HostEvent>) {
    for event in events {
        // Cloned out of the cell before the call: the listener may call setListener.
        let Some(listener) = listener.try_borrow().ok().and_then(|l| l.clone()) else { return };
        let value = match event.to_json().map(|json| js_sys::JSON::parse(&json)) {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => {
                web_sys::console::error_1(&e);
                continue;
            }
            Err(e) => {
                web_sys::console::error_1(&JsValue::from(e));
                continue;
            }
        };
        if let Err(e) = listener.call1(&JsValue::NULL, &value) {
            web_sys::console::error_1(&e);
        }
    }
}
