#!/bin/bash

export YARN_CACHE_FOLDER=$YARN_OUTDIR/yarncache

exec $YARN_OUTDIR/yarn/bin/yarn "$@"
