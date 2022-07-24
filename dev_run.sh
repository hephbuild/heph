#!/bin/bash

set -e

CWD=$(pwd)

cd HEPH_BUILD_ROOT && HEPH_CWD="$CWD" go run . "$@"
