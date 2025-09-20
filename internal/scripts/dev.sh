#!/usr/bin/env bash

set -e

export HEPH_CWD=$(pwd)
export HEPH_SRC_ROOT="<HEPH_SRC_ROOT>"
export GOEXPERIMENT=greenteagc

cd $HEPH_SRC_ROOT

exec go run github.com/hephbuild/heph "$@"
