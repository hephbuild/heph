#!/usr/bin/env bash

set -e

export HEPH_CWD=$(pwd)
export HEPH_SRC_ROOT="<HEPH_SRC_ROOT>"
export RUSTFLAGS="-A unused_imports -A dead_code -A unused"

cd $HEPH_SRC_ROOT

exec cargo run -q -- "$@"
