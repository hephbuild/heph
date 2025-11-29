#!/bin/bash
set -euo pipefail

export WORK=/tmp/hephgenlexer

rm -rf ${WORK}
mkdir -p ${WORK}

VERSION=$(go list -m all | grep github.com/alecthomas/participle/v2 | awk '{print $2}')

git clone --branch ${VERSION} https://github.com/alecthomas/participle.git ${WORK}/participle

GOBIN=$(go env GOBIN)
[[ -d "${GOBIN}" ]] || GOBIN=$(go env GOPATH)/bin
go install -C "${WORK}/participle/cmd/participle" 'github.com/alecthomas/participle/v2/cmd/participle'

go run github.com/hephbuild/heph/lib/tref/internal/lexer/gen > ${WORK}/lexer.json

${GOBIN}/participle gen lexer internal < ${WORK}/lexer.json | gofmt > lexer.gen.go
