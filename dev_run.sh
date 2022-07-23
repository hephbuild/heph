#!/bin/bash

set -e

CWD=$(pwd)

cd HEPH_BUILD_ROOT && HEPH_ROOT="$CWD" go run . "$@"
