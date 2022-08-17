#!/bin/bash

set -e

export GOPATH=${GOPATH:-$GO_OUTDIR/gopath}
export GOCACHE=${GOCACHE:-$GO_OUTDIR/gocache}
export GOFLAGS=-modcacherw
export CGO_ENABLED=0

$GO_OUTDIR/go/bin/go "$@"
