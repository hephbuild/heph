#!/bin/bash

set -ex

VERSION=$1

mkdir -p ./yarn
mkdir -p ./yarncache
curl -L -o- https://github.com/yarnpkg/yarn/releases/download/$VERSION/yarn-$VERSION.tar.gz | tar -xz -C yarn --strip-components=1
