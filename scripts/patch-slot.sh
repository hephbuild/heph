#!/usr/bin/env bash
#
# Stamp fixed-width byte slots into an already-compiled artifact, in place.
#
# `crates/core/src/version.rs` reserves each slot as a `#[used] static`
# initialized to a distinctive marker; this script overwrites the marker
# post-build and the runtime reads it back via a volatile load. Two slots use
# it, for two different reasons:
#
#   flavour  Both release flavours ("std" and "debug") come from ONE compile
#            (see the strip-after-build note on `[profile.release]` in the root
#            Cargo.toml), so no compile-time value can tell them apart.
#
#   version  Keeping the build version OUT of the compile. As an `option_env!`
#            constant it changed on every push (it folds in `git describe` and
#            the CI run number) and `core` sits at the bottom of the dependency
#            graph, so it invalidated every first-party crate and every link
#            above them — measured at ~68% of the build job's compile time,
#            while third-party deps sat at a 95% cache hit rate.
#
# Usage: scripts/patch-slot.sh <artifact> <name>=<value> [<name>=<value> ...]
#   e.g. scripts/patch-slot.sh heph_linux_amd64 version=v1.2.3+gabc flavour=debug
#
# Takes all slots at once rather than one per invocation so macOS is re-signed
# exactly once, after the last write.

set -euo pipefail

bin="${1:?usage: patch-slot.sh <artifact> <name>=<value> [...]}"
shift
if [ "$#" -eq 0 ]; then
  echo "patch-slot: no slots given; usage: patch-slot.sh <artifact> <name>=<value> [...]" >&2
  exit 1
fi

python3 - "$bin" "$@" <<'PY'
import sys

path, assignments = sys.argv[1], sys.argv[2:]

# name -> (marker, slot length). Must match crates/core/src/version.rs.
SLOTS = {
    "flavour": (b"HEPH_FLAVOUR_SLOT_V1", 32),
    "version": (b"HEPH_VERSION_SLOT_V1", 64),
}

with open(path, "rb") as f:
    data = f.read()

for assignment in assignments:
    if "=" not in assignment:
        sys.exit(f"patch-slot: {assignment!r} is not <name>=<value>")
    # split on the FIRST `=` only: a version like `v1+g=abc` must survive.
    name, value = assignment.split("=", 1)
    if name not in SLOTS:
        sys.exit(f"patch-slot: unknown slot {name!r}; known: {', '.join(sorted(SLOTS))}")
    marker, slot_len = SLOTS[name]
    marker_slot = marker.ljust(slot_len, b"\0")

    payload = value.encode("utf-8")
    if len(payload) >= slot_len:
        sys.exit(
            f"patch-slot: {name} value {value!r} ({len(payload)} bytes) "
            f"does not fit the {slot_len}-byte slot"
        )
    new_slot = payload.ljust(slot_len, b"\0")

    # Fail loudly rather than silently patch the wrong (or no) occurrence —
    # either the artifact wasn't built with the slot at all, or the marker
    # collided with unrelated bytes elsewhere, and either way patching blind
    # would be wrong. A missing version slot in particular would ship an
    # artifact reporting `v0.0.0-dev`, which self-upgrade treats as a dev build
    # and refuses to ever upgrade.
    count = data.count(marker_slot)
    if count != 1:
        sys.exit(f"patch-slot: expected exactly one {name} slot in {path}, found {count}")

    data = data.replace(marker_slot, new_slot, 1)
    print(f"patch-slot: {path} {name} slot -> {value!r}")

with open(path, "wb") as f:
    f.write(data)
PY

# Patching bytes in place invalidates a Mach-O's ad-hoc code signature (the
# same reason `scripts/macos-portable.sh` re-signs after `install_name_tool`)
# — without this, Apple Silicon refuses to launch the artifact at all.
if [ "$(uname -s)" = "Darwin" ]; then
  codesign --force --sign - "$bin"
  codesign --verify --strict "$bin"
fi
