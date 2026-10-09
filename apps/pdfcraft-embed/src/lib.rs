//! The embed build: `EmbeddedEditor`, PdfCraft's page view in view mode, mounted by a host page on
//! its own canvas and driven through `pdfcraft_ui_egui::embed` (README.md). Only the browser build
//! has content; `packaging/embed/package.sh` builds the module.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod editor;
#[cfg(target_arch = "wasm32")]
mod types;

#[cfg(target_arch = "wasm32")]
pub use editor::EmbeddedEditor;
