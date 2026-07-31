#!/usr/bin/env bash
#
# Stamp the release flavour into an already-compiled `heph` binary, in place.
#
# Both release flavours ("std" and "debug") come from ONE compile (see the
# strip-after-build note on `[profile.release]` in the root Cargo.toml), so
# there is no compile-time env var to tell them apart — `crates/core/src/version.rs`
# reserves a fixed-width byte slot (`FLAVOUR_SLOT`), initialized to a
# distinctive marker, and this script overwrites it post-build with the real
# flavour. `heph version` / self-upgrade read it back at runtime via
# `hcore::version::flavour()`.
#
# Usage: scripts/patch-flavour.sh <binary> <flavour>
#   <flavour>: "" for std, or a name like "debug". Must fit the 32-byte slot.

set -euo pipefail

bin="${1:?usage: patch-flavour.sh <binary> <flavour>}"
# `${2?...}` (no colon), not `${2:?...}` — the "std" flavour is legitimately
# the empty string, and `:?` treats an empty positional argument as unset.
flavour="${2?usage: patch-flavour.sh <binary> <flavour>}"

python3 - "$bin" "$flavour" <<'PY'
import sys

path, flavour = sys.argv[1], sys.argv[2]
SLOT_LEN = 32
MARKER = b"HEPH_FLAVOUR_SLOT_V1"
marker_slot = MARKER.ljust(SLOT_LEN, b"\0")

payload = flavour.encode("utf-8")
if len(payload) >= SLOT_LEN:
    sys.exit(f"patch-flavour: {flavour!r} ({len(payload)} bytes) does not fit the {SLOT_LEN}-byte slot")
new_slot = payload.ljust(SLOT_LEN, b"\0")

with open(path, "rb") as f:
    data = f.read()

# Fail loudly rather than silently patch the wrong (or no) occurrence — either
# the binary wasn't built with the slot at all, or the marker collided with
# unrelated bytes elsewhere, and either way patching blind would be wrong.
count = data.count(marker_slot)
if count != 1:
    sys.exit(f"patch-flavour: expected exactly one flavour slot in {path}, found {count}")

data = data.replace(marker_slot, new_slot, 1)

with open(path, "wb") as f:
    f.write(data)

print(f"patch-flavour: {path} flavour slot -> {flavour!r}")
PY

# Patching bytes in place invalidates a Mach-O's ad-hoc code signature (the
# same reason `scripts/macos-portable.sh` re-signs after `install_name_tool`)
# — without this, Apple Silicon refuses to launch the binary at all.
if [ "$(uname -s)" = "Darwin" ]; then
  codesign --force --sign - "$bin"
  codesign --verify --strict "$bin"
fi
