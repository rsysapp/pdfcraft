#!/usr/bin/env bash
# Check a built embed zip the way a host would:  packaging/embed/verify-zip.sh <zip>
#
# - <zip>.sha256 is exactly one line, "<hash>  <zip's own file name>", and the hash is the zip's;
# - the zip holds exactly the files the embed API promises (apps/pdfcraft-embed/README.md, a fixed list, plus
#   licenses/OFL-<family>.txt for the fonts of a craft-fonts build) and SHA256SUMS, nothing else;
# - every entry name is a plain relative file name: no absolute path, no ".." component, no
#   backslash, no folder entry, no characters a shell or unzip's wildcards could read differently,
#   at most 100 characters, and no two names that differ only in case (on a case-insensitive file
#   system one would overwrite the other);
# - SHA256SUMS lists every file but itself, once, and each entry's bytes match its listed hash.
#
# Nothing is extracted: each entry is streamed out of the zip into the hasher, so no entry name can
# make this script read or write a file outside the zip. Every refusal prints its reason.
# Needs: unzip, and sha256sum or shasum. Self-test: packaging/embed/test-verify-zip.sh.
set -euo pipefail
# Byte semantics for every pattern and sort below: a caller's locale must not widen [A-Za-z].
export LC_ALL=C

fail() {
  echo "error: $*" >&2
  exit 1
}

# What a build always holds, besides SHA256SUMS. licenses/ is the third-party assets compiled into
# the module (packaging/embed/package.sh); a craft-fonts build adds licenses/OFL-<family>.txt.
REQUIRED=(
  pdfcraft_embed.js pdfcraft_embed_bg.wasm pdfcraft_embed.d.ts
  LICENSE-MIT LICENSE-APACHE NOTICE ATTRIBUTION.md THIRD-PARTY-LICENSES.txt
  licenses/CC0-CGATS001Compat.txt licenses/LICENSE-Adobe-CMap.txt licenses/LICENSE-Foxit-PDFium.txt
  licenses/LICENSE-Hack.txt licenses/LICENSE-emoji-icon-font.txt licenses/LICENSE-hayro-MIT.txt
  licenses/LICENSE-lucide.txt licenses/OFL-DancingScript.txt licenses/OFL-Inter.txt
  licenses/OFL-JetBrainsMono.txt licenses/OFL-NotoEmoji.txt licenses/UFL-Ubuntu.txt
)
SUMS_NAME="SHA256SUMS"

hash_file() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
hash_stdin() {
  if command -v sha256sum >/dev/null; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi
}

zip="${1:-}"
[ -n "$zip" ] || fail "usage: verify-zip.sh <pdfcraft-embed-version.zip>"
[ -f "$zip" ] || fail "$zip is not a file"
[ -f "$zip.sha256" ] || fail "$zip.sha256 is missing"
command -v unzip >/dev/null || fail "unzip is not installed"

# 1. The zip against its sibling .sha256: one line, naming this file and no other.
zip_name="$(basename "$zip")"
sha_lines="$(awk 'END { print NR }' "$zip.sha256")"
[ "$sha_lines" = 1 ] || fail "$zip_name.sha256 has $sha_lines lines; it must be exactly one"
sha_line="$(cat "$zip.sha256")"
if ! [[ "$sha_line" =~ ^([0-9a-f]{64})\ [\ \*](.+)$ ]]; then
  fail "$zip_name.sha256 is not '<sha256>  <file name>'"
fi
sha_hash="${BASH_REMATCH[1]}"
sha_target="${BASH_REMATCH[2]}"
[ "$sha_target" = "$zip_name" ] || fail "$zip_name.sha256 names '$sha_target', not $zip_name"
zip_hash="$(hash_file "$zip")"
[ "$sha_hash" = "$zip_hash" ] || fail "$zip_name does not match its .sha256 (zip $zip_hash, file says $sha_hash)"

# 2. The entry names, each refused for its own reason before any is used as an argument.
entries="$(unzip -Z1 "$zip")" || fail "unzip cannot list $zip_name"
[ -n "$entries" ] || fail "$zip_name is empty"
# The longest name a build writes is about 40 characters; a craft-fonts family name stays well
# under the cap.
MAX_NAME=100
while IFS= read -r entry; do
  [ "${#entry}" -le "$MAX_NAME" ] || fail "entry '${entry:0:$MAX_NAME}...' is longer than $MAX_NAME characters"
  case "$entry" in
    /*) fail "entry '$entry' is an absolute path" ;;
    *\\*) fail "entry '$entry' holds a backslash" ;;
    */) fail "entry '$entry' is a folder entry; the zip holds files only" ;;
  esac
  case "/$entry/" in
    */../* | */./* | *//*) fail "entry '$entry' has a '.', '..' or empty path component" ;;
  esac
  # Wildcard characters, spaces and control characters stay out of names: `unzip -p` reads its name
  # argument as a pattern. The embed's own names never need them.
  [[ "$entry" =~ ^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)?$ ]] || fail "entry '$entry' has characters outside A-Z a-z 0-9 . _ - and one '/'"
done <<<"$entries"

sorted_entries="$(printf '%s\n' "$entries" | LC_ALL=C sort)"
duplicates="$(printf '%s\n' "$sorted_entries" | uniq -d)"
[ -z "$duplicates" ] || fail "the zip lists an entry twice: $(printf '%s' "$duplicates" | tr '\n' ' ')"
case_twins="$(printf '%s\n' "$sorted_entries" | tr '[:upper:]' '[:lower:]' | sort | uniq -d)"
[ -z "$case_twins" ] || fail "the zip holds entries that differ only in case: $(printf '%s' "$case_twins" | tr '\n' ' ')"

# 3. The entry set is the required list plus SHA256SUMS, plus craft-fonts licence texts.
missing=""
for want in "${REQUIRED[@]}" "$SUMS_NAME"; do
  printf '%s\n' "$sorted_entries" | grep -qxF -- "$want" || missing="$missing $want"
done
[ -z "$missing" ] || fail "the zip lacks:$missing"
unexpected=""
while IFS= read -r entry; do
  known=0
  for want in "${REQUIRED[@]}" "$SUMS_NAME"; do
    if [ "$entry" = "$want" ]; then known=1; fi
  done
  if [[ "$entry" =~ ^licenses/OFL-[A-Za-z0-9._-]+\.txt$ ]]; then known=1; fi
  if [ "$known" = 0 ]; then unexpected="$unexpected $entry"; fi
done <<<"$sorted_entries"
[ -z "$unexpected" ] || fail "the zip holds files that are not part of the embed build:$unexpected"

# 4. SHA256SUMS: "<hash>  <name>" (or "<hash> *<name>"), one line per file, none for itself.
sums="$(unzip -p "$zip" "$SUMS_NAME")" || fail "cannot read $SUMS_NAME from $zip_name"
listed_names=""
listed_pairs=""
while IFS= read -r line; do
  if ! [[ "$line" =~ ^([0-9a-f]{64})\ [\ \*](.+)$ ]]; then
    fail "$SUMS_NAME has a line that is not '<sha256>  <file name>': '$line'"
  fi
  listed_names="$listed_names${BASH_REMATCH[2]}"$'\n'
  listed_pairs="$listed_pairs${BASH_REMATCH[1]} ${BASH_REMATCH[2]}"$'\n'
done <<<"$sums"
sorted_listed="$(printf '%s' "$listed_names" | LC_ALL=C sort)"
duplicates="$(printf '%s\n' "$sorted_listed" | uniq -d)"
[ -z "$duplicates" ] || fail "$SUMS_NAME lists a file twice: $(printf '%s' "$duplicates" | tr '\n' ' ')"
expected_listed="$(printf '%s\n' "$sorted_entries" | grep -vxF -- "$SUMS_NAME")"
if [ "$sorted_listed" != "$expected_listed" ]; then
  echo "error: $SUMS_NAME and the zip's files differ (< listed, > in the zip):" >&2
  diff <(printf '%s\n' "$sorted_listed") <(printf '%s\n' "$expected_listed") >&2 || true
  exit 1
fi

# 5. Each file's bytes, streamed out of the zip, against its listed hash. Names were checked
# against the entry list above, so they hold no wildcard.
while read -r want_hash name; do
  [ -n "$name" ] || continue
  got_hash="$(unzip -p "$zip" "$name" | hash_stdin)" || fail "cannot read $name from $zip_name"
  [ "$got_hash" = "$want_hash" ] || fail "$name does not match $SUMS_NAME (zip $got_hash, listed $want_hash)"
done <<<"$listed_pairs"

echo "ok: $zip_name matches its checksums"
