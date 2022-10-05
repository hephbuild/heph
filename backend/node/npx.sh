#!/bin/bash

export npm_config_cache=$NODE_OUTDIR/nodecache

exec $NODE_OUTDIR/node/bin/node $NODE_OUTDIR/node/lib/node_modules/npm/bin/npx-cli.js --yes "$@"
