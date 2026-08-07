# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Environment

This project uses [devenv](https://devenv.sh) for reproducible development environments. All development should happen inside the devenv shell.

```bash
devenv shell        # enter the dev shell (provides Rust toolchain, buf, protoc plugins)
```

### Build cache

Every `rustc` invocation goes through [kache](https://github.com/kunobi-ninja/kache)
(`RUSTC_WRAPPER`, set in `devenv.nix` — so it applies in CI too, which runs inside
this shell). It replaced sccache, which by design cannot cache "crates that invoke
the system linker" — `bin`, `dylib`, `cdylib`, `proc-macro` — i.e. the `heph`
binary, the three plugin cdylibs, every proc-macro and every test harness.

- **Local** is a local-disk store only, at `~/.cache/kache` (Linux) or
  `~/Library/Caches/kache` (macOS). No remote, and therefore **no daemon needed** —
  the wrapper reads and writes the store directly. On a copy-on-write filesystem
  (APFS, btrfs, XFS-with-reflink) restores are reflinks, so a restored `target/`
  costs almost no additional disk and blobs are shared across worktrees.
- **CI** points the same wrapper at the shared R2 bucket via `KACHE_S3_*` and runs
  the daemon (`.github/actions/setup-nix`). The remote is inert without the daemon:
  it owns remote lookups and background uploads. Each compiling job ends with
  `kache daemon stop`, which drains the upload queue before the runner is torn down.
- To share CI's cache locally, export the same `KACHE_S3_*` vars and run
  `kache daemon start`.

`kache stats` for a summary, `kache monitor` for a live TUI, `kache why-miss <crate>`
to explain a miss, `KACHE_PROGRESS=verbose` for per-crate stderr lines, and
`kache doctor` when something looks wrong. `KACHE_DISABLED=1` bypasses it entirely.

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
e2e --test tui_pty           # one test file
e2e restores_the_terminal    # one test, by name substring
e2e -- --nocapture           # args after `--` reach cargo test
HEPH_E2E_FROM=dist e2e       # test an already-downloaded artifact set (what CI does)
HEPH_E2E_KEEP_DIST=1 e2e     # keep the staged artifacts instead of deleting them
```

Selecting a file needs `--test`. Everything after `e2e` is forwarded to `cargo test` verbatim, so a bare `e2e tui_pty` is a *test-name* filter — and no test in `tui_pty.rs` is named `tui_pty`, so it matches nothing, runs zero tests, and **exits 0**. A run that tested nothing is indistinguishable from a run that passed; always check the `running N tests` count.

One script, one code path — CI runs the same `e2e`, differing only in where the artifacts come from. Do not add a parallel script or inline the steps into the workflow.

Concurrency-safe: the suite stages into a `mktemp -d` unique to each run rather than a fixed path two runs would fight over, and fingerprints `release/` around the copy — if another build lands in that window, the run aborts rather than quietly testing the wrong binary. Keep it that way when editing the script; a fixed staging path reintroduces both.

There is no `CARGO_TARGET_DIR` override anywhere — every workspace uses cargo's own `target/`. Worktrees used to share one target dir so dependencies compiled once; kache does that properly now (keyed on content, shared across worktrees and machines, reflinked so a restored `target/` costs almost no disk), and the shared directory only bought concurrent builds writing to the same path. Scripts that need the path call `target-dir`, which asks cargo — don't reintroduce the env var, and don't assume `$DEVENV_ROOT/target`, which is wrong the moment a shell started in one checkout is used in another.

It builds `--release`, so it is slow and disk-hungry on a cold tree. Don't run it reflexively: the `bin_e2e` CI job runs it on all three platforms on every push, and it gates `release`. Run it locally only when changing something it covers (the loader, the TUI, CLI exit codes, the `e2e` script itself) — and expect a full release build the first time.

`tst` excludes `bin-e2e` deliberately: those tests need staged artifacts and hard-fail without `HEPH_E2E_DIST`, so a suite that never ran can't read as a suite that passed. See `.claude/testing.md` for what belongs there versus `crates/e2e`.

### `heph-bench` — perf-regression harness

`crates/bench-corpus` (deterministic synthetic corpus generator) + `crates/bench` (`heph-bench` binary: `corpus`/`run inprocess`/`run dist`/`compare`). Times `heph` scenarios in-process (Tier A, no process spawn, no plugin cdylib) or against the real prebuilt binary + plugin cdylib (Tier B, the seam only a real `dlopen` can exercise), then decides regression from a baseline-vs-candidate comparison.

**Don't run it locally by default — let CI do it.** It exists to catch regressions across a baseline (N-1) and a candidate (N) build under controlled, repeatable conditions; a single local run competes with everything else on the machine and its noise floor makes one-off numbers unreliable next to CI's comparison. Run it locally only when absolutely necessary — reproducing a CI-reported regression, or developing the harness itself — or when explicitly asked to.

## Workflow

Don't run the full test suite locally — CI runs `tst` on every push, so running it first only delays the push.

1. Make the change, with tests.
2. Run `lint` and the tests relevant to the change (`cargo test <test_name>`).
3. Commit, push, open the PR — if the change depends on an unmerged PR, stack it (see below). CI takes it from there.

The same applies to subsequent pushes on an open PR: push the fix and let CI run the suite.

Run the full `tst` suite locally only for a large blast radius change — one touching the engine core, provider/driver traits, or caching, where a break is likely to be wide rather than local. Run it before opening the PR: the cost of a broken PR there is higher than the wait.

### Stacked PRs

**Stack dependent work**, with the `gh stack` extension (`github/gh-stack`) — a change that cannot compile, or cannot be reviewed on its merits, without the one below it. Everything else branches off `master` in parallel: independent PRs review independently, merge in any order, and one being blocked doesn't block the rest. A recent effort produced ~20 PRs and exactly two of them needed a stack.

```bash
gh stack init -b master feat/base       # start a stack (adopts existing branches, bottom to top)
gh stack add feat/on-top                # start a dependent branch on top of the current stack
gh stack submit --auto                  # push branches, create/update the PRs, link the stack
gh stack view --short                   # the branches and each PR's state
gh stack sync                           # fetch, cascade-rebase onto trunk, atomic force-with-lease push
gh stack rebase                         # cascade rebase only; --continue / --abort for conflicts
gh stack link <pr-url> <pr-url>         # register already-open PRs as a stack, bottom to top, no local tracking
```

Plain `gh stack submit` opens an editor for PR titles — pass `--auto` from a script or an agent. `gh stack sync` aborts instead of prompting when the local and remote stacks have diverged and there is no tty; that is the safe outcome, not a failure.

- **Merge bottom-up, and sync after each merge.** GitHub retargets a child PR at `master` on its own when the base merges, but the branch still carries the base's commits — run `gh stack sync` (or `gh stack rebase`) once the base lands so the PR's diff is its own change again. `master` is squash-only, so the base's commits have no counterpart in trunk after the merge: expect that rebase to conflict, and resolve it under the rule below.
- **A red check on a stacked PR is not necessarily its own.** Before debugging, check the base: `gh pr checks <base-pr>`. Same job red there → not your bug; say so on your PR and fix it in the base, not in yours. This has already cost real time — a stacked PR reddened on a flake inherited from its base, and the fix for it lived in a third PR entirely.
- **Don't fold a fix for the base into your stack.** It muddies the revert line — the fix disappears if your PR is reverted, and it lands bundled with an unrelated change. Fix the base in the base, or in its own PR.
- **After resolving a stack conflict, diff against the lower branch and re-run the *lower* PR's tests.** For each conflicted file, `git diff <lower-branch> -- <file>` and confirm every remaining difference is deliberately yours. A resolution can compile, pass your tests, and still revert the change below you: git applies an upper-PR copy of a moved code block cleanly *above* the conflict region and marks only the code below, so taking the upper side verbatim silently dropped a lower PR's `sort`/`dedup` and put a `HashSet` seed back into a def hash.
- **Stacked PRs do get CI.** `pull_request:` in `.github/workflows/heph.yml` is deliberately unfiltered — a `branches: ["master"]` filter matches the PR's *base*, so stacked PRs got zero runs and an empty check list that reads like a pass (fixed in #240). Don't re-add the filter.

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