---
name: ci-triage
description: >
  Wait for the current branch's CI run and, if it fails, classify each failed job as a known flake,
  an inherited failure from a stacked base, or a real break — from the failed logs saved to a file,
  never pulled whole into context. Reruns flakes, and returns the real breaks with the failing
  test and error line. Trigger when the user says "check CI", "why is CI red", "triage CI", or
  invokes /ci-triage [<pr-number or run-id>].
context: fork
model: sonnet
---

# CI triage

Output: one line per failed job — `FLAKE (rerun)`, `INHERITED (base #N)`, or `REAL` with the
test name, the error line and `file:line`. No log dumps. Do not fix anything.

## 1. Find the run and wait for it

```bash
BRANCH=$(git branch --show-current)
RUN=$(gh run list --branch "$BRANCH" --workflow heph.yml --limit 1 --json databaseId,headSha --jq '.[0].databaseId')
```

Check the run's `headSha` against `git rev-parse HEAD`. Right after a push the new run can
take a minute to appear, and `gh pr checks` meanwhile prints "no checks reported", which is not a pass.

Wait with `gh run watch "$RUN" --exit-status`, using `run_in_background`, never a sleep loop. Read the
result from `gh run view "$RUN" --json conclusion,jobs`, not from the watcher alone.

If the run was skipped, the PR is stacked (its base is not `master`): the `gate` job skips it,
so no checks ran. The fix is the `ci/force-ci` label, not a rerun.

## 2. Save the failed logs to a file

```bash
LOG=<scratchpad dir>/ci-$RUN.log
gh run view "$RUN" --log-failed > "$LOG"
grep -anE '^test .* FAILED|panicked at|^error(\[|:)|Diff in |os error|dns error|close_notify|timed out' "$LOG" | head -60
```

Use `-a`, because sandbox output can put NUL bytes in the log, and grep would then print nothing,
which reads as a pass.

## 3. Classify, in this order

1. **Stacked base.** `gh pr view --json baseRefName`. If the base is not `master`, run `gh pr checks <base-pr>`.
   If the same job is red there, the failure is **INHERITED**. Say so; the fix belongs in the base.
2. **Known flakes.** If the signature matches, rerun the failed jobs with `gh run rerun "$RUN" --failed`:

   | Signature | Where | What it is |
   |---|---|---|
   | `dns error`, `operation timed out`, `peer closed connection without sending TLS close_notify` on an outbound fetch (go.dev SDK, `hephbuild/heph-artifacts-v1` release assets) | `Test darwin/arm64`, Linux green | macOS runner network drop |
   | `top_level_resolution_is_admission_controlled` or another test with a wall-clock budget | `Test darwin/arm64` | slow runner; `master` shows it too |
   | `No space left on device (os error 28)` | `plugingo-e2e`, locally under full `tst` | disk pressure. Check `df -h .` and rerun the one test alone |

   Before calling something a flake, check whether `master` is green: `gh run list --branch master --workflow heph.yml --limit 3`.
   A flake is red on one platform with the other two green. **The same failure on all three is never a flake.**
3. **Known real-but-confusing.** These are REAL:
   - `unrecognized subcommand` from `documented_examples_are_accepted`, red on all three `Test` jobs: a clap doc comment has a line starting with `heph `. See `src/commands/gendocs.rs`.
   - `Diff in <file>` in Lint: rustfmt. The branch was pushed without a passing `lint`.
   - Lint red in CI but green locally after a `devenv.nix` edit: the local shell's `lint` is stale. Start a new `devenv shell`.
   - `libunwind: malformed DW_CFA_def_cfa_register DWARF unwind, reg too big` and a test binary dying with SIGSEGV on `Test darwin/arm64`, the same binary on every rerun: a binary linked by the old ld64, which truncates unwind offsets past 16 MiB of `__eh_frame`. `tests/root/darwin_linker.rs` should be red too. See the linker note in `.cargo/config.toml`. A rerun never fixes it.
   - An unrelated test breaking after a dependency was added: check `git diff master -- Cargo.lock | grep -c '^-version'`. A re-resolved lockfile moves existing pins.
   - `ld.lld: error: undefined symbol: ` with an **empty** name, `>>> referenced by … in archive …/libaws_lc_sys-*.rlib`, on the Linux `Build` jobs only while the debug `Test` jobs stay green: the objects inside `libaws_lc_sys-*.rlib` come out with undefined `STT_SECTION` symbols, only under the nixpkgs rolling 2026-10-12 pin (LLVM 21.1.8 built by gcc 16.2). It is the gcc-16 lld bug NixOS/nixpkgs#571232 (`ld.lld -r` writes `STT_SECTION` symbols with `SHN_UNDEF`; fixed by NixOS/nixpkgs#568680, merged to staging 2026-10-07, not in nixpkgs-unstable as of 2026-10-19) reached through zig: every `zig cc -c` ends with an `ld.lld -r` over clang's object (`zig build-obj --verbose-link` prints it; the result carries a section symbol for every section, `.comment` and `.llvm_addrsig` included, which a clang object never does), and nixpkgs' zig links nixpkgs' lld. So it bites exactly the jobs that compile C through `cargo zigbuild` — the Linux `Build` jobs, both targets — and never `cargo build` (the `Test` jobs, darwin), where the cc crate runs clang directly and the objects are clean; that is why `cargo build -p aws-lc-sys` on the same runner reproduces nothing. Not kache, not rustc, not the aws-lc version: #503–#507 swapped each one and only the nixpkgs revision mattered. Our report is NixOS/nixpkgs#575843. `devenv.lock` holds the nixpkgs input at the last good revision; a PR that bumps it checks that #568680 has reached nixpkgs-unstable (and that zig was rebuilt against the fixed lld) first. A rerun never fixes it.
4. **Anything else is REAL.** Report the test, the first error line and its `file:line`, then stop.

## 4. Report

```
run <id> (<sha>): <conclusion>
- Test darwin/arm64: FLAKE — dns error fetching heph-govet → rerun started
- Lint linux/amd64: REAL — Diff in crates/engine/src/engine/query.rs:212 (rustfmt)
```
