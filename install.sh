#!/bin/bash

set -e

OUT="${INF_OUT:-/usr/local/bin/heph}"

BASE="https://storage.googleapis.com/heph-build"

VERSION=$(curl -fsSL ${BASE}/latest_version)

GOOS=$(echo $(uname -s) | tr '[:upper:]' '[:lower:]')
GOARCH=$(echo $(uname -m) | tr '[:upper:]' '[:lower:]')

if [ "$GOARCH" == "x86_64" ]; then
   GOARCH="amd64"
fi

curl -o "${OUT}" ${BASE}/${VERSION}/heph_${GOOS}_${GOARCH}
chmod +x "${OUT}"
