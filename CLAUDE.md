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

tst                                  # run all tests (excludes bin-e2e)
e2e                                  # binary end-to-end suite (see below)
lint                                 # lint
fix                                  # format & apply lint fixes
gen                                  # regenerate protobuf bindings (runs buf generate)
```

The `gen` script is a devenv-provided alias, assume its present. It must be run at the beginning of all sessions, or after any `.proto` file changes before building.

### `e2e` — testing the shipped binary

`e2e` runs `crates/bin-e2e`: black-box tests that spawn the **release binary and plugin cdylibs** as a child process, rather than linking the crates. It is the only way to cover things that have no in-process form — `dlopen` of a real cdylib, the TUI under a PTY, process exit codes, whether the binary launches at all on this host.

```bash
e2e                          # build the artifacts from this tree, then test them
e2e tui_pty                  # one test file
e2e -- --nocapture           # args after `--` reach cargo test
HEPH_E2E_FROM=dist e2e       # test an already-downloaded artifact set (what CI does)
HEPH_E2E_KEEP_DIST=1 e2e     # keep the staged artifacts instead of deleting them
```

One script, one code path — CI runs the same `e2e`, differing only in where the artifacts come from. Do not add a parallel script or inline the steps into the workflow.

Concurrency-safe: `CARGO_TARGET_DIR` is inherited from the environment and worktrees routinely share one, so the suite stages into a `mktemp -d` unique to each run rather than a fixed path two runs would fight over. It also fingerprints `release/` around the copy — if another worktree's build lands in that window, the run aborts rather than quietly testing the wrong binary. Keep it that way when editing the script; a fixed staging path reintroduces both.

It builds `--release`, so it is slow and disk-hungry on a cold tree. Don't run it reflexively: the `bin_e2e` CI job runs it on all three platforms on every push, and it gates `release`. Run it locally only when changing something it covers (the loader, the TUI, CLI exit codes, the `e2e` script itself) — and expect a full release build the first time.

`tst` excludes `bin-e2e` deliberately: those tests need staged artifacts and hard-fail without `HEPH_E2E_DIST`, so a suite that never ran can't read as a suite that passed. See `.claude/testing.md` for what belongs there versus `crates/e2e`.

## Workflow

Don't run the full test suite locally — CI runs `tst` on every push, so running it first only delays the push.

1. Make the change, with tests.
2. Run `lint` and the tests relevant to the change (`cargo test <test_name>`).
3. Commit, push, open the PR. CI takes it from there.

The same applies to subsequent pushes on an open PR: push the fix and let CI run the suite.

Run the full `tst` suite locally only for a large blast radius change — one touching the engine core, provider/driver traits, or caching, where a break is likely to be wide rather than local. Run it before opening the PR: the cost of a broken PR there is higher than the wait.

## Review Board

Standing agents (`.claude/agents/`) own quality for this project. They are advisory — they return verdicts, you implement.

### Always consult

For any non-trivial feature or change. Skip for typo fixes, comment edits, and mechanical renames.

| Agent | Owns | Consult at |
|---|---|---|
| `product-vision` | Is it the right thing; fast/easy/useful for humans *and* agents; CLI surface, naming, **diagnosability** ("why did it do that?" is a design-time requirement) | **Design** (before writing code), and again on the finished UX |
| `feature-quality` | Test coverage, corner cases, and the low-overhead promise (memory, disk, CPU, allocations, per-target cost) | **Design** (what will this cost?) and **review** (is it tested, is it cheap?) |
| `code-quality` | Correctness, soundness, Rust idiom, code smells, wheel-reinvention | **Review**, before commit |

### Consult when triggered

Mechanical triggers — if the change touches it, consult. Not a judgment call.

| Agent | Trigger | Consult at |
|---|---|---|
| `hermeticity` | New/changed `Driver` or `Provider`; anything feeding the input hash, def hash, or cache key; sandbox input/output declaration | **Design** and **review** |
| `compatibility` | `proto/`; `crates/plugin-abi` or `ABI_SEMVER`; cache serialization / on-disk or remote-cache format; Starlark builtins or rule signatures; CLI command/flag/exit-code/`--json` shape | **Design** (before the format is fixed) and **review** |
| `perf-measurement` | Change lands on a hot path (result/spec resolution, hashing, cache read, provider walk); a perf claim needs proof; something feels slower | **After** implementation, before commit |

### Rules

- Consults at the same stage run in parallel — one message, multiple agents.
- A **BLOCKER** from `feature-quality`, `code-quality`, `hermeticity`, or `compatibility` is fixed, or explicitly overruled with a stated reason, before the commit.
- **NOT HERMETIC** and **BREAKING** are never silently accepted — either fix, or record the decision in the commit body.
- A **RETHINK** / **DON'T BUILD** from `product-vision` goes back to the user, not around them.
- **Per-platform behavior is the user's decision.** The supported set is `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin` — no BSD, no Windows, no 32-bit — and features work uniformly across all three by default, on the OS axis (Linux vs macOS) and the arch axis (x86_64 vs aarch64) alike. A divergence may be the right answer — but it is never settled by an agent or by the implementation. Flag it (what differs, on which target, what each option costs) and put the call to the user before writing the code. CI runs the suite natively on all three supported targets, so a green CI does cover an arch-conditional change — but the `linux/arm64` *release* binary is cross-compiled while its test job builds natively, so a toolchain-specific break there is still uncovered.
- **Adding a dependency is allowed** when it gets the job done reliably. A maintained crate beats a fragile hand-rolled version. What still gets flagged: duplicating a crate already in the tree, a second copy of an ecosystem (async runtime, TLS, HTTP client, allocator), hot-path or startup cost, and support limited to one OS or one arch.

@.claude/rust.md
@.claude/testing.md
@.claude/architecture.md