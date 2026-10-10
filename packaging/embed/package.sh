#!/usr/bin/env bash
# Build the embed build and zip it:  $DIST/pdfcraft-embed-<version>.zip (+ .zip.sha256)
#
# Usage: packaging/embed/package.sh [--print-version]
#
# The version is apps/pdfcraft-embed's own (released as tags embed-v<version>), not the app's.
# Needs: the wasm32-unknown-unknown target; wasm-bindgen at the version in Cargo.lock
# (cargo install wasm-bindgen-cli --version <it> --locked, or set WASM_BINDGEN); wasm-opt from
# binaryen (or set WASM_OPT); cargo-about at the version in cargo-about.env, and jq and python3
# (see third-party-licenses.sh). The zip holds, at its root, the files the embed API lists.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"

CRATE="$ROOT/apps/pdfcraft-embed"
EMBED_VERSION="$(awk -F '"' '/^version = "/ { print $2; exit }' "$CRATE/Cargo.toml")"
if [ "${1:-}" = "--print-version" ]; then
  echo "$EMBED_VERSION"
  exit 0
fi
[ -n "$EMBED_VERSION" ] || { echo "error: no version in $CRATE/Cargo.toml" >&2; exit 1; }

WASM_BINDGEN="${WASM_BINDGEN:-wasm-bindgen}"
WASM_OPT="${WASM_OPT:-wasm-opt}"
LOCKED="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$ROOT/Cargo.lock")"
HAVE="$("$WASM_BINDGEN" --version 2>/dev/null | awk '{ print $2 }')" || true
if [ "$HAVE" != "$LOCKED" ]; then
  echo "error: wasm-bindgen ${HAVE:-not found}; Cargo.lock has $LOCKED (cargo install wasm-bindgen-cli --version $LOCKED --locked, or set WASM_BINDGEN)" >&2
  exit 1
fi
command -v "$WASM_OPT" >/dev/null || { echo "error: wasm-opt not found (binaryen; or set WASM_OPT)" >&2; exit 1; }

NAME="pdfcraft-embed-$EMBED_VERSION"
WORK="$CARGO_TARGET_DIR/embed-package"
OUT="$WORK/$NAME"
rm -rf "$WORK"
mkdir -p "$WORK/bindgen" "$OUT"

# The licence notices of the Rust crates and the standard library compiled into the module. First,
# because it fails on a crate with an unknown or unaccepted licence, or a placeholder in its
# notice, and a missing tool shows before the long build.
"$ROOT/packaging/embed/third-party-licenses.sh" "$OUT/THIRD-PARTY-LICENSES.txt"

# This package alone: building it beside the apps would turn the `brand` feature back on.
cargo build --release --locked --target wasm32-unknown-unknown -p pdfcraft-embed

"$WASM_BINDGEN" --target web --typescript --out-dir "$WORK/bindgen" --out-name pdfcraft_embed \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/pdfcraft_embed.wasm"
# As trunk's data-wasm-opt="z" for the web app: wasm-opt reads the module's own feature list.
"$WASM_OPT" -Oz -o "$OUT/pdfcraft_embed_bg.wasm" "$WORK/bindgen/pdfcraft_embed_bg.wasm"
cp "$WORK/bindgen/pdfcraft_embed.js" "$WORK/bindgen/pdfcraft_embed.d.ts" "$OUT/"

# The declarations the embed API promises (generated, so checked rather than trusted).
for want in \
  'export default function __wbg_init' \
  'export class EmbeddedEditor' \
  'static mount(canvas: HTMLCanvasElement, options: MountOptions): Promise<EmbeddedEditor>;' \
  'open(bytes: Uint8Array, fileName: string): void;' \
  'jumpTo(target: JumpTarget): void;' \
  'setMarks(marks: Mark[]): void;' \
  'setTheme(theme: Theme): void;' \
  'setListener(listener: (event: EditorEvent) => void): void;' \
  'destroy(): void;' \
  'export interface MountOptions' \
  'export interface Theme' \
  'export type JumpTarget' \
  'export interface Mark' \
  'export type EditorEvent'; do
  grep -qF -- "$want" "$OUT/pdfcraft_embed.d.ts" || { echo "error: pdfcraft_embed.d.ts lacks: $want" >&2; exit 1; }
done
# docs/brand/LICENSE-brand.txt: a modified build carries none of the ArtCraft logo images and shows
# no ArtCraft names or links. The logos are embedded under these asset keys, so finding one means the
# `brand` feature came back on (the module was built beside the apps). Help strings that spell the
# name stay in the binary, unreachable in view mode, so the name alone is not searched for.
if grep -aq 'bytes://artcraft-' "$OUT/pdfcraft_embed_bg.wasm"; then
  echo "error: the ArtCraft logo images are in the module; build pdfcraft-embed on its own (no brand feature)" >&2
  exit 1
fi

cp "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$ROOT/ATTRIBUTION.md" "$OUT/"
{
  echo "PdfCraft embed build $EMBED_VERSION: a modified version of PdfCraft, based on PdfCraft by the"
  echo "ArtCraft team. It carries none of the ArtCraft logo images and shows no ArtCraft names or links."
  echo "The licence texts of the third-party assets compiled into it (fonts, icons, CMaps, colour profiles)"
  echo "are in licenses/ in this bundle; the paths below are those of the PdfCraft source tree."
  echo
  cat "$ROOT/NOTICE"
} >"$OUT/NOTICE"

# The licence texts of the third-party assets compiled into the module (NOTICE and ATTRIBUTION.md
# name them): published name <- source. Registry crates are found through cargo's own resolution,
# since their folder differs per machine.
METADATA="$(cargo metadata --format-version 1 --locked --filter-platform wasm32-unknown-unknown)"
crate_dir() {
  local dir
  dir="$(printf '%s' "$METADATA" | grep -o "\"manifest_path\":\"[^\"]*/$1-[0-9][^\"/]*/Cargo.toml\"" | head -n 1 \
    | sed -e 's/^"manifest_path":"//' -e 's#/Cargo.toml"$##')"
  [ -n "$dir" ] || { echo "error: $1 is not among the module's dependencies" >&2; exit 1; }
  echo "$dir"
}
CMAP="$(crate_dir hayro-cmap)"
EGUI_FONTS="$(crate_dir epaint_default_fonts)/fonts"
mkdir -p "$OUT/licenses"
while read -r published source; do
  cp "$source" "$OUT/licenses/$published"
done <<EOF_LICENCES
LICENSE-lucide.txt $ROOT/assets/icons/LICENSE-lucide.txt
OFL-Inter.txt $ROOT/assets/fonts/OFL-Inter.txt
OFL-JetBrainsMono.txt $ROOT/assets/fonts/OFL-JetBrainsMono.txt
OFL-DancingScript.txt $ROOT/assets/fonts/OFL-DancingScript.txt
LICENSE-Foxit-PDFium.txt $ROOT/vendor/hayro-interpret/assets/LICENSE_FOXIT
LICENSE-hayro-MIT.txt $ROOT/vendor/hayro-interpret/LICENSE-MIT
CC0-CGATS001Compat.txt $ROOT/vendor/hayro-interpret/assets/CGATS_LICENSE.txt
LICENSE-Adobe-CMap.txt $CMAP/assets/LICENSE.txt
UFL-Ubuntu.txt $EGUI_FONTS/UFL.txt
LICENSE-Hack.txt $EGUI_FONTS/Hack-Regular.txt
OFL-NotoEmoji.txt $EGUI_FONTS/OFL.txt
LICENSE-emoji-icon-font.txt $EGUI_FONTS/emoji-icon-font-mit-license.txt
EOF_LICENCES
# A build with craft-fonts (CRAFT_FONTS_DIR) embeds those fonts too.
copy_font_licences "$OUT/licenses"

FILES=(pdfcraft_embed.js pdfcraft_embed_bg.wasm pdfcraft_embed.d.ts LICENSE-MIT LICENSE-APACHE NOTICE ATTRIBUTION.md THIRD-PARTY-LICENSES.txt)
while IFS= read -r licence; do
  FILES+=("licenses/$licence")
done < <(cd "$OUT/licenses" && find . -maxdepth 1 -type f | sed 's#^\./##' | LC_ALL=C sort)
(
  cd "$OUT"
  # Byte order, not the locale's, so every machine writes the same file.
  for f in $(printf '%s\n' "${FILES[@]}" | LC_ALL=C sort); do
    printf '%s  %s\n' "$(sha256 "$f")" "$f"
  done
) >"$OUT/SHA256SUMS"

# Same bytes in, same zip out: fixed timestamps, no extra attributes, a fixed file order, no
# folder entries (-D). TZ=UTC: zip stores local time and touch reads it, so the zone must not
# matter. chmod: zip stores the mode, so the umask must not matter either.
(cd "$OUT" && chmod 644 "${FILES[@]}" SHA256SUMS && TZ=UTC touch -t 198001010000 "${FILES[@]}" SHA256SUMS)
rm -f "$DIST/$NAME.zip" "$DIST/$NAME.zip.sha256"
# shellcheck disable=SC2046  # the names have no spaces; sorted in byte order for a fixed entry order
(cd "$OUT" && TZ=UTC zip -qX9D "$DIST/$NAME.zip" $(printf '%s\n' "${FILES[@]}" SHA256SUMS | LC_ALL=C sort))
printf '%s  %s\n' "$(sha256 "$DIST/$NAME.zip")" "$NAME.zip" >"$DIST/$NAME.zip.sha256"
echo "wrote $DIST/$NAME.zip"
ls -lh "$DIST/$NAME.zip"
