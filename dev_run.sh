#!/bin/bash

set -e

export HEPH_CWD=$(pwd)
export HEPH_SRC_ROOT="<HEPH_SRC_ROOT>"

cd $HEPH_SRC_ROOT

HEPH_SOURCE=/tmp/heph_source
if [ -f "$HEPH_SOURCE" ]; then
    set -a
    source "$HEPH_SOURCE"
    set +a
fi

exec go run github.com/hephbuild/heph/cmd/heph "$@"
