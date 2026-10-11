#!/usr/bin/env bash
# Self-test for rustflags-guard.sh:  packaging/embed/test-rustflags-guard.sh
#
# Writes cargo configs in a temporary folder and asserts that the guard refuses the rustflags that
# can apply to the wasm build, allows the rest, and scans the ancestor and cargo home paths.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=rustflags-guard.sh
. "$HERE/rustflags-guard.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

failures=0
fail() { echo "FAIL: $1" >&2; failures=$((failures + 1)); }

# check <name> <allowed|refused> <expected message part> <config text>
check() {
  local name="$1" expect="$2" part="$3" text="$4" file="$WORK/$1.toml" status=0 message
  printf '%s\n' "$text" >"$file"
  message="$(rustflags_guard_file "$file" 2>&1)" || status=$?
  if [ "$expect" = allowed ]; then
    [ "$status" -eq 0 ] || fail "$name: expected allowed, got: $message"
    [ -z "$message" ] || fail "$name: allowed but printed: $message"
  else
    [ "$status" -eq 1 ] || fail "$name: expected refused (status 1), got status $status"
    case "$message" in
      *"$file"*"$part"*"would be replaced"*) ;;
      *) fail "$name: message lacks the file, '$part' or 'would be replaced': $message" ;;
    esac
  fi
}

check host-linker allowed '' '[target.x86_64-unknown-linux-gnu]
rustflags = ["-C","link-arg=-fuse-ld=mold"]'
check apple-silicon allowed '' "[target.aarch64-apple-darwin]
rustflags = [\"-C\", \"link-arg=-fuse-ld=lld\"]"
check alias-only allowed '' '[alias]
xtask = "run --package xtask --"
b = "build"'
check other-triple-quoted allowed '' "[target.'x86_64-pc-windows-msvc']
rustflags = [\"-C\", \"target-feature=+crt-static\"]"
check dotted-other-triple allowed '' 'target.x86_64-unknown-linux-gnu.rustflags = ["-C","link-arg=-fuse-ld=mold"]'
check comment-only allowed '' '# rustflags = ["-C", "opt-level=3"]
# [build]'
check rustflags-in-other-section allowed '' '[env]
rustflags = "not a cargo setting"
[build]
jobs = 4'
check array-continuation allowed '' '[alias]
long = [
  "rustflags = x",
]'
check section-then-other-triple allowed '' '[build]
jobs = 4
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C","link-arg=-fuse-ld=mold"]'

check build-section refused '[build]' '[build]
rustflags = ["-C", "opt-level=3"]'
check build-dotted refused 'build.rustflags' 'build.rustflags = ["-C", "opt-level=3"]'
check build-inline refused 'build.rustflags' 'build = { rustflags = ["-C", "opt-level=3"] }'
check wasm-section refused '[target.wasm32-unknown-unknown]' '[target.wasm32-unknown-unknown]
rustflags = ["-C", "opt-level=3"]'
check wasm-dotted refused 'target.wasm32-unknown-unknown.rustflags' 'target.wasm32-unknown-unknown.rustflags = ["-C", "opt-level=3"]'
check wasm-quoted-section refused '[target.wasm32-unknown-unknown]' '[target."wasm32-unknown-unknown"]
rustflags = ["-C", "opt-level=3"]'
check wasm-quoted-dotted refused 'target.wasm32-unknown-unknown.rustflags' 'target."wasm32-unknown-unknown".rustflags = []'
check wasm-in-target-table refused 'wasm32-unknown-unknown.rustflags' '[target]
wasm32-unknown-unknown.rustflags = ["-C", "opt-level=3"]'
check cfg-section refused 'cfg(target_arch = "wasm32")' "[target.'cfg(target_arch = \"wasm32\")']
rustflags = [\"-C\", \"opt-level=3\"]"
check cfg-unix refused "a cfg() expression may match" "[target.'cfg(unix)']
rustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]"
check cfg-dotted refused 'cfg(unix)' "target.'cfg(unix)'.rustflags = []"
check spaced-header refused '[build]' '  [ build ]   # comment
  rustflags   =   ["-C", "opt-level=3"]'
check after-allowed-section refused '[build]' '[target.x86_64-unknown-linux-gnu]
rustflags = ["-C","link-arg=-fuse-ld=mold"]
[build]
rustflags = ["-C", "opt-level=3"]'

# The scan: a project with allowed configs passes; a bad config in an ancestor or the cargo home
# is found; a bad config in an unrelated folder is not read.
project="$WORK/tree/a/project"
mkdir -p "$project/.cargo" "$WORK/tree/.cargo" "$WORK/home" "$WORK/unrelated/.cargo"
printf '[alias]\nx = "build"\n' >"$project/.cargo/config.toml"
printf '[target.x86_64-unknown-linux-gnu]\nrustflags = ["-C","link-arg=-fuse-ld=mold"]\n' >"$WORK/home/config.toml"
printf '[build]\nrustflags = ["-C", "opt-level=3"]\n' >"$WORK/unrelated/.cargo/config.toml"
rustflags_guard_scan "$project" "$WORK/home" 2>/dev/null || fail "scan: allowed configs were refused"
printf '[build]\nrustflags = ["-C", "opt-level=3"]\n' >"$WORK/tree/.cargo/config"
message="$(rustflags_guard_scan "$project" "$WORK/home" 2>&1)" && fail "scan: an ancestor's rustflags were allowed"
case "$message" in *"$WORK/tree/.cargo/config"*) ;; *) fail "scan: message does not name the ancestor file: $message" ;; esac
rm "$WORK/tree/.cargo/config"
printf '[target.wasm32-unknown-unknown]\nrustflags = []\n' >"$WORK/home/config"
message="$(rustflags_guard_scan "$project" "$WORK/home" 2>&1)" && fail "scan: the cargo home's rustflags were allowed"
case "$message" in *"$WORK/home/config"*) ;; *) fail "scan: message does not name the cargo home file: $message" ;; esac

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "rustflags guard: all cases pass"
