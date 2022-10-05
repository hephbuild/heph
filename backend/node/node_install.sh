#!/bin/bash

set -ex

VERSION="$1"

case $ARCH in
    amd64)
        ARCH="x64"
        ;;
esac

mkdir -p ./node
mkdir -p ./nodecache
curl -L -o- https://nodejs.org/dist/$VERSION/node-$VERSION-$OS-$ARCH.tar.gz | tar -xz -C node --strip-components=1
