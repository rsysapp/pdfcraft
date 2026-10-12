//! The app the runner drives: PdfCraft in view mode, plus handing its events to the host.

use std::cell::RefCell;
use std::rc::Rc;

use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::embed::{HostBridge, HostEvent};
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::wasm_bindgen;

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
        // A save that cannot be handed over is answered `saveFailed`: a save never goes unanswered
        // (Ruling W5-1).
        let value = match event_value(&event) {
            Ok(value) => value,
            Err(error) if matches!(event, HostEvent::SaveRequested { .. }) => {
                web_sys::console::error_1(&JsValue::from(error));
                match event_value(&HostEvent::SaveFailed { message: "The document could not be handed over.".into() }) {
                    Ok(value) => value,
                    Err(_) => continue,
                }
            }
            Err(error) => {
                web_sys::console::error_1(&JsValue::from(error));
                continue;
            }
        };
        if let Err(e) = listener.call1(&JsValue::NULL, &value) {
            web_sys::console::error_1(&e);
        }
    }
}

/// The event as the object the host's listener receives. A save's bytes travel as a `Uint8Array`
/// the host owns (a copy outside the module's memory), not as JSON numbers. Allocating the array
/// goes through a `catch` import, so a page that cannot allocate it answers `Err`, not a trap.
fn event_value(event: &HostEvent) -> Result<JsValue, String> {
    let json = event.to_json()?;
    let value = js_sys::JSON::parse(&json).map_err(|error| format!("an event could not be read back: {error:?}"))?;
    if let HostEvent::SaveRequested { bytes, .. } = event {
        let length = u32::try_from(bytes.len()).map_err(|_| format!("a save of {} bytes is too large to hand over", bytes.len()))?;
        let array: js_sys::Uint8Array =
            OwnedBytes::new(length).map(Into::into).map_err(|error| format!("{length} bytes could not be allocated: {error:?}"))?;
        // Same length by construction, so `copy_from`'s length check holds.
        array.copy_from(bytes);
        js_sys::Reflect::set(&value, &JsValue::from_str("bytes"), &array)
            .map_err(|error| format!("a save's bytes could not be attached: {error:?}"))?;
    }
    Ok(value)
}

#[wasm_bindgen]
extern "C" {
    /// A `Uint8Array` made by [`OwnedBytes::new`].
    #[wasm_bindgen(extends = js_sys::Uint8Array, js_name = Uint8Array)]
    type OwnedBytes;

    /// `new Uint8Array(length)`, answering what it throws (a `RangeError` when the page cannot
    /// allocate it). js-sys's own constructors do not catch.
    #[wasm_bindgen(catch, constructor, js_class = "Uint8Array")]
    fn new(length: u32) -> Result<OwnedBytes, JsValue>;
}
