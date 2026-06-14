#!/usr/bin/env bash
#
# Make a nix-built macOS binary portable.
#
# The nix toolchain hard-links system dylibs (e.g. libiconv) against their
# /nix/store/<hash>-.../lib/*.dylib paths instead of the OS /usr/lib copies.
# On any machine without that exact nix store path, dyld aborts the process at
# launch — before any code runs. (libfuse is handled separately: build.rs
# weak-links it, so its absence is tolerated.)
#
# This rewrites every non-weak /nix/store/*.dylib load command to the matching
# /usr/lib/<basename>, which macOS always ships. If a referenced dylib has no
# /usr/lib equivalent, we fail loudly rather than ship a binary that aborts on
# someone else's mac.
#
# Usage: scripts/macos-portable.sh <binary>

set -euo pipefail

bin="${1:?usage: macos-portable.sh <binary>}"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "macos-portable: not on macOS, nothing to do" >&2
  exit 0
fi

# otool -L lines look like:  \t/nix/store/...//lib/libiconv.2.dylib (compat...)
# Skip weak entries — dyld already tolerates those missing.
nix_deps=$(otool -L "$bin" | awk '/\/nix\/store\// && !/\(.*weak.*\)/ {print $1}')

if [ -z "$nix_deps" ]; then
  echo "macos-portable: no /nix/store dylib references in $bin"
  exit 0
fi

for dep in $nix_deps; do
  base=$(basename "$dep")
  sys="/usr/lib/$base"
  # /usr/lib dylibs live in the dyld shared cache and may not exist as files,
  # so we can't stat them. dlopen is the reliable presence check.
  if ! /usr/bin/python3 - "$sys" <<'PY'
import ctypes, sys
try:
    ctypes.CDLL(sys.argv[1])
except OSError:
    raise SystemExit(1)
PY
  then
    echo "macos-portable: ERROR no system equivalent for $dep (expected $sys)" >&2
    echo "macos-portable: refusing to ship a non-portable binary" >&2
    exit 1
  fi
  echo "macos-portable: $dep -> $sys"
  install_name_tool -change "$dep" "$sys" "$bin"
done

echo "macos-portable: done"
otool -L "$bin"

# Regression guard: nothing non-weak may still point into the nix store, or the
# binary aborts on any machine without that exact store path.
remaining=$(otool -L "$bin" | awk '/\/nix\/store\// && !/\(.*weak.*\)/ {print $1}')
if [ -n "$remaining" ]; then
  echo "macos-portable: ERROR non-weak /nix/store refs survived:" >&2
  echo "$remaining" >&2
  exit 1
fi

# `install_name_tool` rewrites the Mach-O after the linker embedded its ad-hoc
# code signature, leaving the signature stale. On Apple Silicon the kernel
# refuses to launch a binary whose signature doesn't match its bytes
# ("Killed: 9" / AMFI invalid signature), so re-sign ad-hoc as the final step
# and verify before handing the binary off.
codesign --force --sign - "$bin"
codesign --verify --strict "$bin"
echo "macos-portable: re-signed ad-hoc"
