#!/bin/bash

# npx is slightly broken: https://github.com/npm/npm/issues/4603

# Extracts PATH from npm
NPATH="$(npm exec -c 'echo $PATH')"

if [ $? -ne 0 ]; then
  NPATH="./node_modules/.bin:$PATH"
fi

export PATH=$NPATH

exec "$@"
