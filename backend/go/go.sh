#!/bin/bash

export GOPATH=${GOPATH:-$GO_SHARED/path}
export GOCACHE=${GOCACHE:-$GO_SHARED/cache}
export GOROOT=$GO_OUTDIR/go
export CGO_ENABLED=${CGO_ENABLED:-0}

set -u

exec $GO_OUTDIR/go/bin/go "$@"
