#!/usr/bin/env bash
# Refuse cargo config files whose rustflags would apply to the wasm build:  . rustflags-guard.sh
#
# package.sh sets CARGO_ENCODED_RUSTFLAGS, which cargo prefers over every rustflags in a config
# file, so such flags would be dropped without a word. Only flags that can reach a
# `--target wasm32-unknown-unknown` build matter, so these are refused:
#   - `rustflags` in [build] (also written `build.rustflags = ...`);
#   - `rustflags` in [target.wasm32-unknown-unknown] (also `target.wasm32-unknown-unknown.rustflags`);
#   - `rustflags` in any [target.'cfg(...)'] section: a cfg expression may match wasm32 and this
#     script cannot evaluate one, so it counts as applying.
# Rustflags for another explicit target triple (a host linker such as mold, say) never apply to
# the wasm build and are allowed. TOML is read by a small line-based awk state machine (no TOML
# library): it follows [section] headers and dotted and quoted keys, and does not parse values.
# Needs only awk and bash 3.2.

# rustflags_guard_file <config>: return 0 when the file is absent or its rustflags cannot apply to
# the wasm build; otherwise print the reason on stderr and return 1.
rustflags_guard_file() {
  local config="$1" hit kind where
  [ -f "$config" ] || return 0
  hit="$(awk -f /dev/stdin "$config" <<'AWK'
# Splits the dotted key starting at s[from] into PATH_PARTS[1..PATH_COUNT], unquoting each part.
# Stops at the first stop character outside quotes (PATH_END is its index, 0 when there is none).
function parse_path(s, from, stop,    i, ch, quote, piece, len) {
  PATH_COUNT = 0; PATH_END = 0; piece = ""; quote = ""; len = length(s)
  for (i = from; i <= len; i++) {
    ch = substr(s, i, 1)
    if (quote != "") {
      if (ch == quote) quote = ""; else piece = piece ch
    } else if (ch == "\"" || ch == "'") {
      quote = ch
    } else if (ch == ".") {
      PATH_PARTS[++PATH_COUNT] = piece; piece = ""
    } else if (ch == stop) {
      PATH_END = i; break
    } else if (ch != " " && ch != "\t") {
      piece = piece ch
    }
  }
  PATH_PARTS[++PATH_COUNT] = piece
}
function joined(parts, count,    i, text) {
  text = parts[1]
  for (i = 2; i <= count; i++) text = text "." parts[i]
  return text
}
BEGIN { section_count = 0; in_array_table = 0 }
{
  line = $0
  sub(/^[ \t]+/, "", line)
  if (line == "" || substr(line, 1, 1) == "#") next
  if (substr(line, 1, 2) == "[[") { in_array_table = 1; section_count = 0; next }
  if (substr(line, 1, 1) == "[") {
    parse_path(line, 2, "]")
    in_array_table = 0
    section_count = 0
    if (PATH_END == 0) { in_array_table = 1; next }
    for (i = 1; i <= PATH_COUNT; i++) SECTION[++section_count] = PATH_PARTS[i]
    next
  }
  if (in_array_table) next
  parse_path(line, 1, "=")
  if (PATH_END == 0) next
  value = substr(line, PATH_END + 1)
  full_count = 0
  for (i = 1; i <= section_count; i++) FULL[++full_count] = SECTION[i]
  for (i = 1; i <= PATH_COUNT; i++) FULL[++full_count] = PATH_PARTS[i]
  # An inline table (`build = { rustflags = [...] }`) sets rustflags one level down.
  if (FULL[full_count] != "rustflags" && value ~ /^[ \t]*\{/ && value ~ /rustflags[ \t]*=/) FULL[++full_count] = "rustflags"
  if (FULL[full_count] != "rustflags") next
  kind = ""
  if (full_count == 2 && FULL[1] == "build") kind = "wasm"
  else if (full_count == 3 && FULL[1] == "target") {
    if (FULL[2] == "wasm32-unknown-unknown") kind = "wasm"
    else if (substr(FULL[2], 1, 4) == "cfg(") kind = "cfg"
  }
  if (kind == "") next
  if (PATH_COUNT == 1 && section_count > 0) where = "in the [" joined(SECTION, section_count) "] section"
  else where = "as " joined(FULL, full_count)
  print kind "\t" where
  exit
}
AWK
)"
  [ -n "$hit" ] || return 0
  kind="${hit%%$'\t'*}"
  where="${hit#*$'\t'}"
  if [ "$kind" = cfg ]; then
    echo "error: $config sets rustflags $where; a cfg() expression may match wasm32-unknown-unknown and this script cannot evaluate it, so those flags would be replaced by this build's CARGO_ENCODED_RUSTFLAGS; move them to a [target.<triple>] section of a non-wasm triple or pass them in RUSTFLAGS" >&2
  else
    echo "error: $config sets rustflags $where, which applies to the wasm32-unknown-unknown build and would be replaced by this build's CARGO_ENCODED_RUSTFLAGS; pass them in RUSTFLAGS instead" >&2
  fi
  return 1
}

# rustflags_guard_scan <root> <cargo home>: check the cargo configs cargo would read for a build in
# <root> (.cargo/config.toml and .cargo/config in it and every ancestor, then the cargo home's);
# return 1 at the first that sets rustflags the wasm build would lose.
rustflags_guard_scan() {
  local config_dir="$1" cargo_home="$2" config
  while :; do
    for config in "$config_dir/.cargo/config.toml" "$config_dir/.cargo/config"; do
      rustflags_guard_file "$config" || return 1
    done
    [ "$config_dir" != / ] || break
    config_dir="$(dirname "$config_dir")"
  done
  for config in "$cargo_home/config.toml" "$cargo_home/config"; do
    rustflags_guard_file "$config" || return 1
  done
  return 0
}
