#!/usr/bin/env bash

set -e

export HEPH_CWD=$(pwd)
export HEPH_SRC_ROOT="<HEPH_SRC_ROOT>"
export GOEXPERIMENT=greenteagc

cd $HEPH_SRC_ROOT

if [ -n "${HEPH_BUILD_WRAP:-}" ]; then
  BUILD_TMP=/tmp/heph_dev_build

  "${HEPH_BUILD_WRAP}" go build -o "${BUILD_TMP}" github.com/hephbuild/heph

	exec "${BUILD_TMP}" "$@"
else
	exec go run github.com/hephbuild/heph "$@"
fi
