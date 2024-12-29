#!/usr/bin/env bash

set -e

export HEPH_CWD=$(pwd)
export HEPH_SRC_ROOT="<HEPH_SRC_ROOT>"

cd $HEPH_SRC_ROOT

exec go run github.com/hephbuild/heph "$@"
