#!/bin/bash

export GOPATH=${GOPATH:-$GO_OUTDIR/gopath}
export GOCACHE=${GOCACHE:-$GO_OUTDIR/gocache}
export GOFLAGS=-modcacherw
export CGO_ENABLED=${CGO_ENABLED:-0}

set -u

exec $GO_OUTDIR/go/bin/go "$@"
