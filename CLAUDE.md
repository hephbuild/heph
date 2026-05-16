# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Environment

This project uses [devenv](https://devenv.sh) for reproducible development environments. All development should happen inside the devenv shell.

```bash
devenv shell        # enter the dev shell (provides Rust toolchain, buf, protoc plugins)
```

## Commands

```bash
cargo build                          # build
cargo test <test_name>               # run a single test by name

tst                                  # run all tests
lint                                 # lint
fix                                  # format & apply lint fixes
gen                                  # regenerate protobuf bindings (runs buf generate)
```

The `gen` script is a devenv-provided alias, assume its present. It must be run at the beginning of all sessions, or after any `.proto` file changes before building.

@.claude/rust.md
@.claude/testing.md
@.claude/architecture.md