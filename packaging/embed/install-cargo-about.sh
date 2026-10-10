#!/usr/bin/env bash
# Installs the pinned cargo-about (cargo-about.env) from its GitHub release, checking the tarball
# against the pinned SHA-256 first. For CI on x86_64 Linux; elsewhere:
#   cargo install cargo-about --locked --features cli --version <the pinned one>
# Adds the install folder to GITHUB_PATH when it runs under GitHub Actions.
set -euo pipefail
# shellcheck source=cargo-about.env
. "$(dirname "${BASH_SOURCE[0]}")/cargo-about.env"

[ "$(uname -s)-$(uname -m)" = "Linux-x86_64" ] || { echo "error: the prebuilt cargo-about is for x86_64 Linux only" >&2; exit 1; }

TEMP_DIR="${RUNNER_TEMP:-$(mktemp -d)}"
NAME="cargo-about-${CARGO_ABOUT_VERSION}-x86_64-unknown-linux-musl"
curl -fsSL -o "$TEMP_DIR/$NAME.tar.gz" "https://github.com/EmbarkStudios/cargo-about/releases/download/${CARGO_ABOUT_VERSION}/$NAME.tar.gz"
echo "$CARGO_ABOUT_LINUX_X86_64_SHA256  $TEMP_DIR/$NAME.tar.gz" | sha256sum -c -
tar -xzf "$TEMP_DIR/$NAME.tar.gz" -C "$TEMP_DIR"
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$TEMP_DIR/$NAME" >>"$GITHUB_PATH"
else
  echo "installed: $TEMP_DIR/$NAME/cargo-about (add the folder to PATH)"
fi
