//! `EmbeddedEditor`: the API a host page calls. Values arrive as JavaScript objects and are read
//! as JSON by the bridge's parsers, so malformed input throws here and queues nothing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::embed::{self, HostBridge, HostCommand};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsError, JsValue};

use crate::app::{EmbedApp, Listener};

#[wasm_bindgen]
pub struct EmbeddedEditor {
    runner: eframe::WebRunner,
    bridge: HostBridge,
    listener: Listener,
    destroyed: Cell<bool>,
}

#[wasm_bindgen]
impl EmbeddedEditor {
    /// Starts the editor on the canvas. Resolves once the first frame ran.
    #[wasm_bindgen(unchecked_return_type = "Promise<EmbeddedEditor>")]
    pub fn mount(canvas: web_sys::HtmlCanvasElement, #[wasm_bindgen(unchecked_param_type = "MountOptions")] options: JsValue) -> js_sys::Promise {
        wasm_bindgen_futures::future_to_promise(async move { start(canvas, options).await.map(JsValue::from) })
    }

    /// Opens a document, replacing any open one. Answers with an `opened` or `openFailed` event.
    pub fn open(
        &self,
        #[wasm_bindgen(unchecked_param_type = "Uint8Array")] bytes: JsValue,
        #[wasm_bindgen(js_name = "fileName")] file_name: String,
    ) -> Result<(), JsError> {
        // Taken as a handle, not `Vec<u8>`: wasm-bindgen would copy the whole array into wasm memory
        // before this body runs, after `destroy` and beyond the size cap alike. A failed allocation
        // aborts the module.
        let bytes = bytes.dyn_into::<js_sys::Uint8Array>().map_err(|_| JsError::new("open: bytes must be a Uint8Array"))?;
        if self.destroyed.get() {
            return Ok(());
        }
        let len = usize::try_from(bytes.length()).unwrap_or(usize::MAX);
        // Copied once, only when the size is taken, then moved into the queue.
        self.send(embed::open_command(file_name, len, || bytes.to_vec()));
        Ok(())
    }

    #[wasm_bindgen(js_name = jumpTo)]
    pub fn jump_to(&self, #[wasm_bindgen(unchecked_param_type = "JumpTarget")] target: JsValue) -> Result<(), JsError> {
        check_counts(&target, &["page", "occurrence"], "jumpTo")?;
        let target = embed::parse_jump_target(&json_of(&target)?).map_err(|e| JsError::new(&e))?;
        self.send(HostCommand::JumpTo(target));
        Ok(())
    }

    /// Replaces every host mark. An empty array clears them.
    #[wasm_bindgen(js_name = setMarks)]
    pub fn set_marks(&self, #[wasm_bindgen(unchecked_param_type = "Mark[]")] marks: JsValue) -> Result<(), JsError> {
        if js_sys::Array::is_array(&marks) {
            for mark in marks.unchecked_ref::<js_sys::Array>().iter() {
                check_counts(&mark, &["page"], "setMarks")?;
            }
        }
        let marks = embed::parse_marks(&json_of(&marks)?).map_err(|e| JsError::new(&e))?;
        self.send(HostCommand::SetMarks(marks));
        Ok(())
    }

    #[wasm_bindgen(js_name = setTheme)]
    pub fn set_theme(&self, #[wasm_bindgen(unchecked_param_type = "Theme")] theme: JsValue) -> Result<(), JsError> {
        let theme = embed::parse_theme(&json_of(&theme)?).map_err(|e| JsError::new(&e))?;
        self.send(HostCommand::SetTheme(theme));
        Ok(())
    }

    /// One listener; a second call replaces the first. Events are delivered after the frame that
    /// produced them.
    #[wasm_bindgen(js_name = setListener)]
    pub fn set_listener(&self, #[wasm_bindgen(unchecked_param_type = "(event: EditorEvent) => void")] listener: JsValue) -> Result<(), JsError> {
        // Taken as a plain value and checked: wasm-bindgen does not check a `Function` argument, and
        // a non-function would only fail later, inside event delivery.
        let listener = listener.dyn_into::<js_sys::Function>().map_err(|_| JsError::new("setListener: expected a function"))?;
        if !self.destroyed.get()
            && let Ok(mut slot) = self.listener.try_borrow_mut()
        {
            *slot = Some(listener);
        }
        Ok(())
    }

    /// Stops the runner and frees the canvas. The object is unusable afterwards: later calls queue
    /// nothing and deliver nothing (malformed arguments still throw), and a second `destroy` does
    /// nothing.
    pub fn destroy(&self) {
        if self.destroyed.replace(true) {
            return;
        }
        shut_down(&self.runner, &self.bridge, &self.listener);
    }
}

/// wasm-bindgen's `free()`, `[Symbol.dispose]()` and garbage collection drop the object without
/// calling `destroy`. The runner's event listeners hold their own copies of it, so without this
/// the editor would keep rendering on the canvas, unreachable. `destroy` is idempotent, so
/// `destroy()` then `free()` is fine, and both take the same path, safe mid-frame (`shut_down`).
impl Drop for EmbeddedEditor {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Stop an editor. The host hears nothing more from the moment this returns: the bridge is
/// detached (its queues and its copy of the egui context dropped, and nothing queued after) and the
/// listener cleared. Only the runner's teardown waits.
///
/// Host code can run inside a frame: eframe focuses and blurs its text agent mid-frame, which
/// dispatches `focusin`/`focusout` that bubble to the host page, and an opened link calls
/// `window.open`, which can fire `blur`. A host calling `destroy()` or `free()` from such a handler
/// would reach `WebRunner::destroy` while the frame holds the runner borrowed, and its
/// `RefCell::replace` panics, aborting the module. So the teardown runs as a task: wasm-bindgen's
/// task queue runs from a microtask, which starts only once the JavaScript stack is empty, so after
/// the frame (and the host's handler) returned. Tasks run in the order spawned, so a `mount` made
/// right after `destroy` starts after the old runner is gone. Until then the app may finish its
/// frame, and its events go nowhere.
fn shut_down(runner: &eframe::WebRunner, bridge: &HostBridge, listener: &Listener) {
    bridge.detach();
    // `try_borrow_mut`, not `replace`: this also runs from `Drop`, which must not panic. Event
    // delivery clones the listener out before calling it, so the cell is free here anyway.
    if let Ok(mut slot) = listener.try_borrow_mut() {
        *slot = None;
    }
    let runner = runner.clone();
    wasm_bindgen_futures::spawn_local(async move { runner.destroy() });
}

impl EmbeddedEditor {
    fn send(&self, command: HostCommand) {
        if !self.destroyed.get() {
            self.bridge.send(command);
        }
    }
}

async fn start(canvas: web_sys::HtmlCanvasElement, options: JsValue) -> Result<EmbeddedEditor, JsValue> {
    let theme = embed::parse_mount_options(&json_of(&options)?).map_err(|e| JsValue::from(JsError::new(&e)))?;
    // A fresh bridge per mount: a bridge keeps the first context it is attached to.
    let bridge = HostBridge::default();
    bridge.send(HostCommand::SetTheme(theme));
    let listener: Listener = Rc::new(RefCell::new(None));
    let mut resolve_first_frame = None;
    let first_frame = js_sys::Promise::new(&mut |resolve, _reject| resolve_first_frame = Some(resolve));
    let runner = eframe::WebRunner::new();
    let (app_bridge, app_listener) = (bridge.clone(), listener.clone());
    let web_options = eframe::WebOptions { renderer: eframe::Renderer::Glow, ..Default::default() };
    let started = runner
        .start(
            canvas,
            web_options,
            Box::new(move |cc| {
                embed::prepare_context(&cc.egui_ctx);
                Ok(Box::new(EmbedApp {
                    inner: PdfCraftApp::embedded(app_bridge.clone()),
                    bridge: app_bridge,
                    listener: app_listener,
                    first_frame: resolve_first_frame,
                }))
            }),
        )
        .await;
    // No `EmbeddedEditor` exists yet to drop, so a start that fails here stops whatever the runner
    // installed, the same way `destroy` does.
    if let Err(error) = started {
        shut_down(&runner, &bridge, &listener);
        return Err(error);
    }
    if let Err(error) = wasm_bindgen_futures::JsFuture::from(first_frame).await {
        shut_down(&runner, &bridge, &listener);
        return Err(error);
    }
    Ok(EmbeddedEditor { runner, bridge, listener, destroyed: Cell::new(false) })
}

/// A JavaScript value as JSON, for the bridge's parsers.
fn json_of(value: &JsValue) -> Result<String, JsError> {
    js_sys::JSON::stringify(value).ok().and_then(|s| JsValue::from(s).as_string()).ok_or_else(|| JsError::new("expected a plain object or array"))
}

/// Refuses a page or occurrence that is not a whole number of zero or more, on the JavaScript
/// value. It must happen before [`json_of`]: `JSON.stringify` writes NaN and ±Infinity as `null`,
/// which the parsers read as "no page", so a page-scoped jump or mark would quietly search the
/// whole document. (The parsers refuse fractions and negatives too; this says so earlier.) A value
/// that is not an object is left to the parsers, which refuse its shape.
fn check_counts(value: &JsValue, keys: &[&str], what: &str) -> Result<(), JsError> {
    if !value.is_object() {
        return Ok(());
    }
    for key in keys {
        let field = js_sys::Reflect::get(value, &JsValue::from_str(key)).map_err(|_| JsError::new(&format!("{what}: {key} could not be read")))?;
        if let Some(number) = field.as_f64()
            && !(number.is_finite() && number.fract() == 0.0 && number >= 0.0)
        {
            return Err(JsError::new(&format!("{what}: {key} {number} is not a whole number of zero or more")));
        }
    }
    Ok(())
}
