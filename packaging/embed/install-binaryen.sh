#!/usr/bin/env bash
# Installs the pinned binaryen (tools.env) from its GitHub release, checking the tarball against
# the pinned SHA-256 first. For CI on x86_64 Linux; elsewhere install that release's wasm-opt by
# hand (apps/pdfcraft-embed/README.md, "Reproducing a release").
# Adds the bin folder to GITHUB_PATH when it runs under GitHub Actions.
set -euo pipefail
# shellcheck source=tools.env
. "$(dirname "${BASH_SOURCE[0]}")/tools.env"

[ "$(uname -s)-$(uname -m)" = "Linux-x86_64" ] || { echo "error: the prebuilt binaryen is for x86_64 Linux only" >&2; exit 1; }

TEMP_DIR="${RUNNER_TEMP:-$(mktemp -d)}"
NAME="binaryen-version_${BINARYEN_VERSION}"
curl -fsSL -o "$TEMP_DIR/$NAME-x86_64-linux.tar.gz" "https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/$NAME-x86_64-linux.tar.gz"
echo "$BINARYEN_LINUX_X86_64_SHA256  $TEMP_DIR/$NAME-x86_64-linux.tar.gz" | sha256sum -c -
tar -xzf "$TEMP_DIR/$NAME-x86_64-linux.tar.gz" -C "$TEMP_DIR"
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$TEMP_DIR/$NAME/bin" >>"$GITHUB_PATH"
else
  echo "installed: $TEMP_DIR/$NAME/bin/wasm-opt (add the folder to PATH)"
fi
