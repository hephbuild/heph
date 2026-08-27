#!/usr/bin/env bash
# Publish the heph plugin SDK and its dependency closure to crates.io.
#
# crates.io cannot vendor path deps, so the closure ships as a renamed chain
# (`heph-*`). This script stages the publish set into an isolated virtual
# workspace, rewrites the manifests (rename + version + metadata), then publishes
# in dependency order. `cargo publish` blocks until each crate is in the index
# before returning, so the next crate's dependency requirement resolves.
#
# Prereqs: `gen` has run (gen/proto present); cargo + a crates.io token in
# CARGO_REGISTRY_TOKEN. Set PYTHON to pick the interpreter (defaults to python3).
#
# Usage: publish-crates.sh <version> [--dry-run]
set -euo pipefail

VERSION="${1:?usage: publish-crates.sh <version> [--dry-run]}"
DRYRUN="${2:-}"
PYTHON="${PYTHON:-python3}"

REPO="https://github.com/hephbuild/heph"
LICENSE="AGPL-3.0-only"

# Dependency (topological) publish order: deps before dependents.
ORDER=(
  heph-core
  heph-model
  heph-htspec-derive
  heph-proto-gen
  heph-plugin
  heph-driver-support
  heph-plugin-abi
  heph-plugin-stabby
  heph-plugin-sdk
)

# Crate source dirs under crates/ to stage (proto-gen is staged from gen/proto).
CRATES=(core model htspec-derive plugin driver-support plugin-abi plugin-stabby plugin-sdk)

ROOT="$(git rev-parse --show-toplevel)"
SCRIPTS="$ROOT/scripts/publish"
cd "$ROOT"

if [[ ! -f gen/proto/Cargo.toml ]]; then
  echo "error: gen/proto missing — run 'gen' before publishing." >&2
  exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/crates" "$STAGE/gen"

echo "==> staging publish set into $STAGE"
for c in "${CRATES[@]}"; do
  cp -R "crates/$c" "$STAGE/crates/$c"
done
cp -R gen/proto "$STAGE/gen/proto"

# Synthetic workspace root + manifest rewrites.
"$PYTHON" "$SCRIPTS/make_staging_root.py" Cargo.toml > "$STAGE/Cargo.toml"
"$PYTHON" "$SCRIPTS/rewrite_manifests.py" "$STAGE" "$VERSION" "$REPO" "$LICENSE"

# Ship the license inside every crate package.
for c in "${CRATES[@]}"; do
  cp LICENSE.md "$STAGE/crates/$c/LICENSE.md"
done
cp LICENSE.md "$STAGE/gen/proto/LICENSE.md"

# A clean .git-less tree means `cargo package` includes the generated proto
# sources (otherwise gitignored). Fail loudly if the rewrite produced a bad
# manifest before we touch the registry.
echo "==> validating staged workspace"
( cd "$STAGE" && cargo metadata --no-deps --format-version 1 >/dev/null )

# A dry run can only ever exercise the leaf crates via `cargo publish
# --dry-run`: a dependent's `heph-*` deps aren't on the registry yet, so cargo's
# pre-upload resolution fails for them (the real publish works only because each
# crate reaches the index before the next one builds). So validate the whole
# renamed chain by compiling it from the path-linked staging workspace instead.
if [[ -n "$DRYRUN" ]]; then
  echo "==> dry run: building the renamed chain (uploads skipped)"
  ( cd "$STAGE" && cargo build --workspace )
  echo "==> dry run OK"
  exit 0
fi

for crate in "${ORDER[@]}"; do
  echo "==> publishing $crate@$VERSION"
  # cargo publish blocks until the crate is in the index, so the next crate's
  # dependency requirement resolves against the registry.
  ( cd "$STAGE" && cargo publish -p "$crate" --allow-dirty )
done

echo "==> done"
