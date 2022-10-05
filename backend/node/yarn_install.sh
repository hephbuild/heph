#!/bin/bash

set -ex

mkdir -p ./yarn
mkdir -p ./yarncache
curl -L -o- https://github.com/yarnpkg/yarn/releases/download/v1.22.19/yarn-v1.22.19.tar.gz | tar -xz -C yarn --strip-components=1
