#!/bin/bash

export npm_config_cache=$NODE_SHARED/nodecache

exec $NODE_OUTDIR/node/bin/node $NODE_OUTDIR/node/lib/node_modules/npm/bin/npm-cli.js "$@"
