#!/bin/bash

BIN="$1"

# https://stackoverflow.com/a/2701420/3212099
shift;

exec "./node_modules/.bin/${BIN}" "$@"
