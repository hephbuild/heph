#!/usr/bin/env bash
# Cross-compile heph-govet for one $GOOS/$GOARCH and record its SHA-256.
#
# One platform per invocation so the four builds can run as parallel steps. Each
# writes a `<name>.sha256` sidecar rather than appending to $GITHUB_OUTPUT:
# concurrent appends to that file would interleave. The collector step turns the
# sidecars into job outputs after the barrier.
set -euo pipefail

: "${GOOS:?GOOS must be set}"
: "${GOARCH:?GOARCH must be set}"

dist="$GITHUB_WORKSPACE/dist"
mkdir -p "$dist"

name="heph-govet_${GOOS}_${GOARCH}"
go build -ldflags="-s -w" -o "$dist/$name" .
sha256sum "$dist/$name" | cut -d' ' -f1 > "$dist/$name.sha256"
echo "$name $(cat "$dist/$name.sha256")"
