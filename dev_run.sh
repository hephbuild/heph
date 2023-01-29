#!/bin/bash

set -e

CWD=$(pwd)

cd HEPH_BUILD_ROOT
HEPH_SOURCE=/tmp/heph_source
if [ -f "$HEPH_SOURCE" ]; then
    set -a
    source "$HEPH_SOURCE"
    set +a
fi

export HEPH_CWD="$CWD"
exec go run . "$@"
