#!/usr/bin/env bash
# Writes the licence notices of everything compiled into the embed module:
#   packaging/embed/third-party-licenses.sh <output-file>
#
# package.sh puts the result in the bundle as THIRD-PARTY-LICENSES.txt; CI runs it alone, so a
# licence problem shows before a release. It fails on:
#   - a crate whose licence is unknown or not accepted (about.toml);
#   - an accepted list in about.toml that differs from deny.toml's allowlist;
#   - a crate whose notice still shows a placeholder ("<year>", "<copyright holders>"), unless
#     about-placeholder-allowlist.txt names it;
#   - a cargo-about other than the pinned one (cargo-about.env).
# Needs: cargo-about, jq, python3, and the rust toolchain the module is built with (its standard
# library notices are part of the output).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../env.sh
. "$HERE/../env.sh"
# shellcheck source=cargo-about.env
. "$HERE/cargo-about.env"

OUTPUT="${1:?usage: third-party-licenses.sh <output-file>}"
CRATE="$ROOT/apps/pdfcraft-embed"

for tool in jq python3 rustc; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found" >&2; exit 1; }
done
HAVE_ABOUT="$(cargo about --version 2>/dev/null | awk '{ print $2 }')" || true
if [ "$HAVE_ABOUT" != "$CARGO_ABOUT_VERSION" ]; then
  echo "error: cargo-about ${HAVE_ABOUT:-not found}; this needs $CARGO_ABOUT_VERSION (packaging/embed/install-cargo-about.sh, or cargo install cargo-about --locked --features cli --version $CARGO_ABOUT_VERSION)" >&2
  exit 1
fi

# One policy: about.toml accepts exactly what deny.toml allows.
quoted_list() { # file key: the strings of a multi-line `key = [` array, sorted
  awk -v key="$2" '$0 ~ ("^" key " = \\[$") { inside = 1; next } inside && /^\]/ { inside = 0 } inside { gsub(/[ ",]/, ""); if ($0 != "") print }' "$1" | LC_ALL=C sort
}
if ! diff <(quoted_list "$ROOT/deny.toml" allow) <(quoted_list "$ROOT/about.toml" accepted) >&2; then
  echo "error: about.toml's accepted list differs from deny.toml's allow list (diff above: < deny.toml, > about.toml)" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Only the embed crate's dependency closure for wasm32 (about.toml leaves out build scripts and
# tests). --fail and the accepted list stop the run on a crate whose licence is unknown or not
# accepted. A clarification whose checksum no longer matches is only a warning to cargo-about; the
# placeholder check below catches the text it falls back to.
about_generate() {
  cargo about generate --manifest-path "$CRATE/Cargo.toml" --config "$ROOT/about.toml" \
    --target wasm32-unknown-unknown --locked --fail "$@"
}
about_generate "$HERE/about.hbs" --output-file "$WORK/crates.txt"
about_generate --format json --output-file "$WORK/crates.json"
if ! [ -s "$WORK/crates.txt" ] || ! grep -q '^  - egui ' "$WORK/crates.txt"; then
  echo "error: the licence notices are empty or lack the module's crates" >&2
  exit 1
fi

# A licence text cargo-about had to make up from the standard text, or one that is still a
# template, names no copyright holder.
FLAGGED="$(jq -r '.licenses[] | select((.source_path == null) or (.text | test("<(year|copyright holders?|owner|organization)>"; "i"))) | .used_by[].crate.name' "$WORK/crates.json" | LC_ALL=C sort -u)"
ALLOWED="$(sed -e '/^#/d' -e '/^[[:space:]]*$/d' "$HERE/about-placeholder-allowlist.txt" | awk '{ print $1 }' | LC_ALL=C sort -u)"
UNEXPECTED="$(LC_ALL=C comm -23 <(printf '%s\n' "$FLAGGED") <(printf '%s\n' "$ALLOWED") | sed '/^$/d')"
if [ -n "$UNEXPECTED" ]; then
  echo "error: these crates show a licence text with a placeholder or no real text:" >&2
  while IFS= read -r crate_name; do echo "  $crate_name" >&2; done <<<"$UNEXPECTED"
  echo "Add a [<crate>.clarify] section to about.toml that points at the licence file naming the holder (see its existing sections), or list the crate in packaging/embed/about-placeholder-allowlist.txt with the reason." >&2
  exit 1
fi
STALE="$(LC_ALL=C comm -13 <(printf '%s\n' "$FLAGGED") <(printf '%s\n' "$ALLOWED") | sed '/^$/d')"
[ -z "$STALE" ] || warn "about-placeholder-allowlist.txt names crates that no longer need it: $(echo "$STALE" | tr '\n' ' ')"

# Author e-mail addresses stay out of the crate lists (the licence texts are kept as they are).
awk '/^Used by:$/ { inside = 1; print; next } inside && /^$/ { inside = 0 } inside { gsub(/ *<[^>]*>/, ""); gsub(/ *[A-Za-z0-9._+-]+@[A-Za-z0-9.-]+/, ""); gsub(/\(, */, "("); gsub(/, *\)/, ")"); gsub(/ *\(\)/, "") } { print }' \
  "$WORK/crates.txt" >"$WORK/notices.txt"

# Apache-2.0 asks that a NOTICE file travel with the work. cargo-about reads licence files only,
# so the NOTICE files in the crates' folders are appended here (PdfCraft's own crates are covered
# by NOTICE in the bundle root). None of today's crates ship one; a future one does so unasked.
while IFS= read -r crate_dir; do
  case "$crate_dir" in "$ROOT"/crates/* | "$ROOT"/apps/*) continue ;; esac
  while IFS= read -r notice; do
    {
      printf '\n--------------------------------------------------------------------------------\n'
      printf 'NOTICE of %s\n' "$(basename "$crate_dir")"
      printf -- '--------------------------------------------------------------------------------\n\n'
      cat "$notice"
    } >>"$WORK/notices.txt"
  done < <(find "$crate_dir" -maxdepth 1 -type f \( -iname 'NOTICE' -o -iname 'NOTICE.*' \) | LC_ALL=C sort)
done < <(jq -r '.crates[].package.manifest_path' "$WORK/crates.json" | sed 's#/Cargo.toml$##' | LC_ALL=C sort -u)

# The Rust standard library (core, alloc, std and what they are built from) is precompiled with
# the toolchain and linked into the module. The toolchain ships its notices as HTML, plus the texts
# of the licences it names; the same pinned toolchain gives the same files.
RUST_DOC="$(rustc --print sysroot)/share/doc/rust"
[ -f "$RUST_DOC/COPYRIGHT-library.html" ] || { echo "error: $RUST_DOC/COPYRIGHT-library.html not found (the rustc component)" >&2; exit 1; }
python3 -I "$HERE/html-to-text.py" "$RUST_DOC/COPYRIGHT-library.html" "$WORK/rust-library.txt"
{
  printf '\n\n================================================================================\n'
  printf 'RUST STANDARD LIBRARY\n'
  printf '================================================================================\n\n'
  printf 'The module links the Rust standard library (core, alloc, std and the crates it is built\n'
  printf 'from, among them compiler_builtins, dlmalloc and libm), precompiled with the toolchain\n'
  printf '"%s". Its copyright notices follow, as the toolchain\n' "$(rustc --version)"
  printf 'ships them in share/doc/rust/COPYRIGHT-library.html, then the texts of the licences they\n'
  printf 'name.\n\n'
  cat "$WORK/rust-library.txt"
  while IFS= read -r licence_file; do
    licence_id="$(basename "$licence_file" .txt)"
    grep -qE "^License:.*(^|[^A-Za-z0-9.+-])$licence_id([^A-Za-z0-9.+-]|\$)" "$WORK/rust-library.txt" || continue
    printf '\n--------------------------------------------------------------------------------\n'
    printf '%s (as shipped with the Rust toolchain)\n' "$licence_id"
    printf -- '--------------------------------------------------------------------------------\n\n'
    cat "$licence_file"
  done < <(find "$RUST_DOC/licenses" -maxdepth 1 -type f -name '*.txt' | LC_ALL=C sort)
} >>"$WORK/notices.txt"

cp "$WORK/notices.txt" "$OUTPUT"
echo "wrote $OUTPUT ($(jq '[.licenses[].used_by[].crate | "\(.name) \(.version)"] | unique | length' "$WORK/crates.json") crates)"
