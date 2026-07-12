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

## Workflow

Open the PR before running the full test suite — CI runs `tst` anyway, so running it locally first just delays the push.

1. Make the change, with tests.
2. Run `lint` and the tests relevant to the change (`cargo test <test_name>`).
3. Commit, push, open the PR. CI takes it from there.
4. Only then run `tst` locally if you want a faster signal than CI.

The same applies to subsequent pushes on an open PR: push the fix and let CI run the suite, don't run `tst` locally first.

Exception: for a major change — one touching the engine core, provider/driver traits, caching, or anything with wide blast radius — run the full `tst` suite locally *before* opening the PR. The cost of a broken PR is higher than the wait.

@.claude/rust.md
@.claude/testing.md
@.claude/architecture.md