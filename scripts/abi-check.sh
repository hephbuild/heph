#!/usr/bin/env bash
#
# Guard the plugin ABI version.
#
# The in-process stable-ABI plugin transport links a separately-compiled host
# and plugin at load time, so any drift in the shared layout is an ABI break.
# `ABI_SEMVER` (crates/plugin-abi/src/lib.rs) is the contract version. If a PR
# touches the ABI surface but leaves `ABI_SEMVER` unchanged, the host/plugin
# pair would mismatch at load and abort the plugin — so this fails the build.
#
# The full rules (what is a major vs minor bump) live in
# crates/plugin-stabby/ABI_VERSIONING.md. This script enforces the mechanical
# part: surface changed => version must move.
#
# Usage: scripts/abi-check.sh [base-ref]
#   base-ref defaults to origin/master (override in CI with the PR base).

set -euo pipefail

base_ref="${1:-origin/master}"
doc="crates/plugin-stabby/ABI_VERSIONING.md"

# The version constant and the native stabby surface. ANY content change to a
# hard-surface path requires a bump — it may alter vtable/struct layout, which
# is not something we can classify cheaply, so we conservatively force the bump
# and let the author pick major/minor per the doc.
version_file="crates/plugin-abi/src/lib.rs"
hard_paths=(
  "crates/plugin-stabby/src/abi.rs"
)
# stabby pins: a version change reshapes every type report -> hard ABI break.
stabby_pin_files=(
  "crates/plugin-stabby/Cargo.toml"
  "crates/plugin-sdk/Cargo.toml"
  "crates/plugin-go-cdylib/Cargo.toml"
)
# Cold-path wire messages: prost is lenient, additive changes are minor. We warn
# (don't fail) so doc/comment proto edits stay quiet but real edits get a nudge.
soft_glob="proto/plugin/v1/"

# Resolve the merge-base so we compare only what this branch introduced, not
# unrelated commits that landed on the base since it forked.
if ! base_sha=$(git merge-base "$base_ref" HEAD 2>/dev/null); then
  echo "abi-check: cannot resolve merge-base with '$base_ref'; skipping" >&2
  exit 0
fi

changed=$(git diff --name-only "$base_sha" HEAD)

matches() { # path, pattern...  -> true if path appears in $changed
  local p
  for p in "$@"; do
    grep -qxF "$p" <<<"$changed" && return 0
  done
  return 1
}

# Pull the ABI_SEMVER literal out of a given revision (empty if absent).
read_version() { # ref
  git show "$1:$version_file" 2>/dev/null \
    | sed -n 's/.*ABI_SEMVER[^"]*"\([^"]*\)".*/\1/p' \
    | head -n1
}

# Pull the stabby version pin out of a given revision (first match wins; all
# pins are kept in lockstep, so one is representative).
read_stabby() { # ref
  local f
  for f in "${stabby_pin_files[@]}"; do
    git show "$1:$f" 2>/dev/null \
      | sed -n 's/.*stabby[^0-9]*\([0-9][0-9.]*\).*/\1/p' \
      | head -n1 && return 0
  done
}

base_version=$(read_version "$base_sha")
head_version=$(read_version HEAD)
base_stabby=$(read_stabby "$base_sha")
head_stabby=$(read_stabby HEAD)

reasons=()
if matches "${hard_paths[@]}"; then
  reasons+=("native stabby surface changed (${hard_paths[*]})")
fi
if [ "$base_stabby" != "$head_stabby" ]; then
  reasons+=("stabby pin changed ($base_stabby -> $head_stabby)")
fi

# Pre-1.0 escape: an acknowledged intentional break (e.g. redefining the contract
# in place before any plugin is released) carries `ABI-BREAK-ACK` in a commit
# message on the branch. See ABI_VERSIONING.md.
ack=""
if git log --format='%B' "$base_sha"..HEAD 2>/dev/null | grep -q 'ABI-BREAK-ACK'; then
  ack=1
fi

if [ ${#reasons[@]} -gt 0 ] && [ "$base_version" = "$head_version" ]; then
  if [ -n "$ack" ]; then
    echo "abi-check: ABI surface changed, ABI_SEMVER unchanged — ABI-BREAK-ACK present, allowing." >&2
  else
    echo "::error::ABI surface changed but ABI_SEMVER did not move ($head_version)" >&2
    echo "" >&2
    echo "abi-check: the plugin ABI changed and requires a version bump:" >&2
    for r in "${reasons[@]}"; do echo "  - $r" >&2; done
    echo "" >&2
    echo "Bump ABI_SEMVER in $version_file (currently \"$head_version\"), or — for an" >&2
    echo "intentional pre-1.0 redefine — add 'ABI-BREAK-ACK: <reason>' to a commit." >&2
    echo "Major vs minor: see $doc" >&2
    exit 1
  fi
elif [ ${#reasons[@]} -gt 0 ]; then
  echo "abi-check: ABI surface changed; ABI_SEMVER bumped $base_version -> $head_version. OK"
else
  echo "abi-check: no hard ABI surface change. OK"
fi

# Soft surface: any proto/plugin/v1 edit. Non-fatal — but if it added/removed a
# wire field, the author should bump at least the minor (see the doc).
if grep -q "^${soft_glob}" <<<"$changed"; then
  echo "::warning::proto/plugin/v1 changed — if a wire field was added/removed, bump ABI_SEMVER minor (see $doc)" >&2
fi
