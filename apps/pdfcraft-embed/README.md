# pdfcraft-embed

The embed build: PdfCraft's page view as a WebAssembly module that a host page mounts on its own
`<canvas>` and drives through one typed API, `EmbeddedEditor` (`pdfcraft_embed.d.ts`). This version
has view mode only: the page view with its own page navigation, zoom and find. Nothing edits,
saves, prints or opens files: the engine refuses changes (`Session::set_view_only`) and command
dispatch refuses everything but find, page display and zoom. Ctrl/Cmd+C copies the selection.

## Build

```sh
packaging/embed/package.sh   # dist/release/pdfcraft-embed-<version>.zip and .zip.sha256
```

The zip holds `pdfcraft_embed.js` (generated glue, ES module), `pdfcraft_embed_bg.wasm`,
`pdfcraft_embed.d.ts` (generated declarations), `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`,
`ATTRIBUTION.md` and `SHA256SUMS`. A tag `embed-v<version>` releases it
(`.github/workflows/embed-release.yml`); the version is this crate's own, not the app's.
The zip's hash is reproducible on the same toolchain and checkout path (the module records
source paths).

## Behaviour a host page relies on

- Call the module's default export (`init`) before `EmbeddedEditor.mount`. `mount` resolves after
  the first frame, so a canvas in a hidden tab resolves when the tab shows.
- Set the listener before `open`: events with no listener are dropped. Events arrive after the
  frame that produced them. `opened` is followed by `pageChanged` for the first page.
- `open` replaces the document and clears marks and any jump still waiting. A broken file, or one
  that needs a password, answers `openFailed`; there is never a password prompt.
- Text jumps and marks match with whitespace runs as one space, Unicode compatibility forms
  folded (NFKC), curly quotes and dashes straightened and case folded, within one page. A text
  jump waits until the pages it searches have their text; a newer jump replaces a waiting one.
- `selection` is sent when a selection settles (button released, or a keyboard selection);
  `x`, `y` are CSS pixels from the canvas's top-left at the end of its last line.
- Malformed arguments (`jumpTo`, `setMarks`, `setTheme`, `mount` options) throw and change nothing.
- `destroy` stops the runner; later calls do nothing. Dropping the object (`free()`) destroys it
  too.
- Put a timeout on `mount` in the host page: it resolves after the first frame, which a canvas
  that never shows (or a failed start) does not produce.
- Each `mount` installs eframe's panic hook. Mounting and destroying repeatedly in one page chains
  hooks, one per mount.
- Events raised before `setListener` are dropped.
- Keyboard: eframe keeps Tab on the canvas. Wrap the canvas in an element with a capture-phase
  `keydown` listener that moves focus on Tab and Shift+Tab.
- Nothing is stored in the host page's storage.

## Branding

Built without the `brand` feature of `pdfcraft-ui-egui`: the bundle carries none of the ArtCraft
logo images and shows no ArtCraft names or links (`docs/brand/LICENSE-brand.txt`). The package
script fails if a logo asset key (`bytes://artcraft-`) is in the module. Build `pdfcraft-embed`
with `-p pdfcraft-embed` alone: building it together with the apps (`--workspace`, or
`-p pdfcraft-web`) unifies the `brand` feature back on. `NOTICE` in the zip says the build is
based on PdfCraft by the ArtCraft team.
