#!/bin/bash

set -ex

VERSION="$1"

curl -L -o- https://go.dev/dl/go$VERSION.$OS-$ARCH.tar.gz | tar -xz -C go --strip-components=1
rm -rf go/test
