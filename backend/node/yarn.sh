#!/bin/bash

export YARN_CACHE_FOLDER=${YARN_CACHE_FOLDER:-$NODE_SHARED/yarncache}

exec $YARN_OUTDIR/yarn/bin/yarn "$@"
