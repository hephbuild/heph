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

`tst` runs every pass with `--no-fail-fast` and exits non-zero if any failed, so one red test binary no longer hides the rest; extra args pass through to every pass. Its body spelled out as `cargo test --workspace` is the same suite, and the gate holds it to the same `HEPH_FULL_SUITE=1` opt-in.

`lint`, `fix`, `tst` and `e2e` are devenv scripts, and their text is **baked into the shell when it starts**. After editing `devenv.nix`, the running shell still has the old scripts. For example, a new crate added to `qualityCrates` is not format-checked, so `lint` is green locally and red in CI. Start a new `devenv shell` before trusting them.

`fix` can fail with "output file … is not writeable": kache restores `target/` as read-only reflinks. `cargo fmt --all` is the reliable way to format.

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

There is no `CARGO_TARGET_DIR` override anywhere — every workspace uses cargo's own `target/`. Worktrees used to share one target dir so dependencies compiled once; kache does that properly now (keyed on content, shared across worktrees and machines, reflinked so a restored `target/` costs almost no disk), and the shared directory only bought concurrent builds writing to the same path. Scripts that need the path call `target-dir`, which asks cargo — don't reintroduce the env var, and don't assume `$DEVENV_ROOT/target`, which is wrong the moment a shell started in one checkout is used in another. Bare `target-dir` answers for `$DEVENV_ROOT` (for `run-release` against another project); a script that has just run `cargo build` calls `target-dir .`, the workspace it built. A bare one in `e2e` once built a worktree and then tested the main checkout's stale binaries, green; `tests/target_dir_script.rs` now fails on that.

It builds `--release`, so it is slow and disk-hungry on a cold tree. Don't run it reflexively: the `bin_e2e` CI job runs it on all three platforms on every push, and it gates `release`. Run it locally only when changing something it covers (the loader, the TUI, CLI exit codes, the `e2e` script itself) — and expect a full release build the first time.

`tst` excludes `bin-e2e` deliberately: those tests need staged artifacts and hard-fail without `HEPH_E2E_DIST`, so a suite that never ran can't read as a suite that passed. See `.claude/testing.md` for what belongs there versus `crates/e2e`.

### Credentials

`docs/CREDENTIALS.md`. A credential is a target (`driver = "auth.credential"`)
declaring an identity, an ordered chain of ways to obtain it, and the shape it is
presented in; a consumer names it with `credentials = [...]`.

Read the doc before touching any of it. The two things to know going in: the
reference is an `Input` with `hashed: false, runtime: false`, so **nothing about
a credential can reach a cache key** and that exclusion is structural rather than
conventional — and the other half of the contract, that a presentation carries
material and handles but never anything that *selects content*, is a
**convention** the parser cannot check. A target whose output depends on which
identity ran it is not cacheable and says so with `cache = False`.

### Deferred values

`docs/DEFERRED_VALUES.md`. `${read://pkg:name}` in a driver option is the contents
of that target's single output, resolved by the host at run.

Two things to know going in. The host does **all** of it — the walk over
`TargetSpec.config`, the edge it appends after `parse`, the substitution — because
the two obligations a driver could have had both fail silently; a driver's whole
diff is `String` → `Deferred<String>`. And the def hash covers the *unresolved*
reference while `hashin` covers the producer's content, so `heph query` never
builds and the consumer's key derives from what the value was derived from.
A reference is refused in anything that shapes the graph or identity, and a
**runner spec is never deferrable**.

### Exec runners

Every subprocess heph spawns goes through `crates/execrunner`, never
`hproc::proc_exec` directly — `tests/execrunner_gate.rs` fails the build if a
driver starts to. A target can name a runner (`runner = "//tools/devenv:runner"`
on `exec`/`bash`, or the driver's `runner:` option workspace-wide) and its
command runs inside the environment that runner describes.

Read `docs/EXEC_RUNNERS.md` before touching any of it. The one thing to know
going in: a runner target's `fingerprint` is what makes a consumer's cache key
move when the environment does, and getting it wrong is a silently wrong build
in one direction and a permanent full-miss in the other.

### `heph-bench` — perf-regression harness

`crates/bench-corpus` (deterministic synthetic corpus generator) + `crates/bench` (`heph-bench` binary: `corpus`/`run inprocess`/`run dist`/`compare`). Times `heph` scenarios in-process (Tier A, no process spawn, no plugin cdylib) or against the real prebuilt binary + plugin cdylib (Tier B, the seam only a real `dlopen` can exercise), then decides regression from a baseline-vs-candidate comparison.

**Don't run it locally by default — let CI do it.** It exists to catch regressions across a baseline (N-1) and a candidate (N) build under controlled, repeatable conditions; a single local run competes with everything else on the machine and its noise floor makes one-off numbers unreliable next to CI's comparison. Run it locally only when absolutely necessary — reproducing a CI-reported regression, or developing the harness itself — or when explicitly asked to.

## Workflow

**Never run the whole test suite locally — not `tst`, not `cargo test --workspace`, whatever the change.** CI runs it on every push, on all three platforms; a local run only delays the push. That holds for engine-core, trait and caching changes too: a wide break shows up in CI.

Spot-run the tests of the crates the change touched, and of the crates it could have broken:

- **Touched:** every crate the diff changes.
- **Could have broken:** the crates that depend on a touched crate *and* rely on what changed — its public API, a trait's contract, a type's semantics, a default. A change that only moves code inside a function breaks nothing downstream; a changed `Provider`/`Driver` contract can break every provider and driver crate. List the direct dependents, then keep the ones that use the changed item:

  ```bash
  cargo tree --workspace -i <crate> -e normal,dev --depth 1 --prefix none   # who depends on <crate>
  rg -l '<changed_item>' crates src                                       # which of them use it
  ```

  Compile breakage in a dependent already shows in `lint` (clippy over the whole workspace, every target). The test run is for *behavior*: a dependent that still compiles but now does the wrong thing.
- **The end-to-end layer:** the `crates/e2e` / `crates/plugingo-e2e` test files that exercise the change. An engine-semantics change is covered there, not in the crate.

When that set is most of the workspace — a change in `core` or `model` reaches nearly every crate — keep the dependents that exercise the changed behavior and leave the rest to CI. That is still never the whole suite.

```bash
cargo test -p <crate> -p <dependent>      # touched and possibly-broken crates
cargo test -p e2e --test <file>           # the e2e file covering the change
cargo test -p <crate> <test_name>         # one test while iterating
```

1. Make the change, with tests.
2. Run `lint` and the tests of the touched and possibly-broken crates.
3. Commit, push, open the PR — if the change depends on an unmerged PR, stack it (see below). CI takes it from there.

The same applies to subsequent pushes on an open PR: push the fix and let CI run the suite.

**With a review board running, sequence the fixes.** Fix a finding straight away only in code no pending reviewer covers; hold the rest until the slowest reviewer reports, then fix them together. Run `lint` and the tests of the touched and possibly-broken crates **once**, on the SHA you push. A run before the board, or before its last report, is invalidated by every later fix: one session ran the tests twice and still pushed a commit neither run covered, and rewrote one function three times for three reviewers.

A `PreToolUse` hook (`.claude/hooks/gate`, a Go program run with `go run`; tests are `go test` there) enforces this for agents:
- `tst` or `e2e` anywhere in a command is refused. `HEPH_FULL_SUITE=1` lifts the refusal. An agent uses it only for a spot run of `crates/bin-e2e` scoped to one file (`HEPH_FULL_SUITE=1 e2e --test <file>`) when the change touches what it covers (see `e2e` above) — never on `tst`, and never on a bare `e2e`.
- A foreground `sleep` of 30s or more is refused.
- `git push` and `gh stack submit` are refused unless `lint` passed on exactly HEAD's tree.
  - A red Lint job costs a whole CI round-trip to learn what `lint` would have said in a minute.
  - The hook rewrites each `lint` in an agent's command into a call back to itself. That call records the working tree, runs the real `lint`, and writes `<git-dir>/heph-lint-ok` only when `lint` exits 0.
  - Nothing is added to `lint` itself, which CI runs.
  - Any edit or commit after the run means running `lint` again. `HEPH_PUSH_UNLINTED=1` pushes anyway.
- A scripted edit is refused: `sed -i`, `perl -i`, or a python script that reads, replaces and writes. `HEPH_SCRIPTED_EDIT=1` is the opt-in, only for the compiler-verified exception under "Session economy".
- Nothing else is refused, but the model is told its context size once it passes 300k, and again at each further 100k (not inside a subagent).

The hook needs `go` on `PATH` (devenv provides it) and lets everything through without it.

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

- **Fix a lower layer from the top branch; don't switch branches.** `git commit --fixup=<lower-layer-sha>`, then `git rebase --autosquash --update-refs <base>` (no `-i` needed; `--update-refs` moves the layer branches along). Checking out a lower branch to amend it meant a WIP commit, an amend and a rebase per detour — one replayed the old commit and had to be redone with `--onto` — and it switched the tree under review agents and a running suite that were reading it.
- **In a worktree, local `master` is stale.** It is checked out in the main checkout and cannot be fast-forwarded here, so `master...HEAD` in a worktree compares against whatever it was when that checkout last pulled (83 commits behind, once). Use `origin/master` after a `git fetch`, or the lower layer's SHA for a stacked PR.
- **Merge bottom-up, and sync after each merge.** GitHub retargets a child PR at `master` on its own when the base merges, but the branch still carries the base's commits — run `gh stack sync` (or `gh stack rebase`) once the base lands so the PR's diff is its own change again. `master` is squash-only, so the base's commits have no counterpart in trunk after the merge: expect that rebase to conflict, and resolve it under the rule below.
- **A red check on a stacked PR is not necessarily its own.** Before debugging, check the base: `gh pr checks <base-pr>`. Same job red there → not your bug; say so on your PR and fix it in the base, not in yours. This has already cost real time — a stacked PR reddened on a flake inherited from its base, and the fix for it lived in a third PR entirely.
- **Don't fold a fix for the base into your stack.** It muddies the revert line — the fix disappears if your PR is reverted, and it lands bundled with an unrelated change. Fix the base in the base, or in its own PR.
- **After resolving a stack conflict, diff against the lower branch and re-run the *lower* PR's tests.** For each conflicted file, `git diff <lower-branch> -- <file>` and confirm every remaining difference is deliberately yours. A resolution can compile, pass your tests, and still revert the change below you: git applies an upper-PR copy of a moved code block cleanly *above* the conflict region and marks only the code below, so taking the upper side verbatim silently dropped a lower PR's `sort`/`dedup` and put a `HashSet` seed back into a def hash.
- **A `git rebase --skip` drops the whole commit, not just the redundant part.** A constructor added in the same commit as an unrelated removal vanished when that commit was skipped, and two call sites went back to writing a value the reader rejects. A rebase reports that each commit applied, not that each layer still works. After a skip or a non-trivial conflict, check out every branch in the stack and run its lint and tests.
- **A stacked PR does not build until it reaches the bottom.** The `gate` job in `.github/workflows/heph.yml` runs CI for a push, or for a PR whose base is `master`; a PR based on another branch is skipped — every check reports "skipped" and the `Summary` job says why. Add the **`ci/force-ci`** label to build one anyway (labelling starts the run on its own; no push needed). This does not weaken the merge gate: required status checks live in the `master` ruleset, whose condition is `~DEFAULT_BRANCH`, so they apply to exactly the PRs the gate builds. A stacked PR merges with skipped checks because nothing is required of it, and its code still cannot reach `master` without first becoming a `master`-targeting PR, which builds.
- **What the skip costs you: an upper layer's break surfaces late.** A change that only fails on `darwin/arm64`, or only under `--no-default-features`, sits undetected at layer 3 until the two below it land. On a deep stack that serializes debugging into one cycle per layer. `ci/force-ci` is the answer when a layer is worth testing on its own — a large refactor low in the stack, anything platform-specific, a flake hunt. Use it rather than assuming green-when-it-gets-there.
- **Sync after the base merges.** GitHub retargets the child at `master` itself, and that retarget *is* an `edited` event, so it now starts a full run — but the tree it builds still carries the base's commits. `gh stack sync` force-pushes the rebased branch (`synchronize` → another run), and that is the run whose result means anything. You need the rebase regardless, since `master` is squash-only.
- **Don't add a `branches:` filter to `pull_request:`.** It matches the PR's *base*, so stacked PRs got zero runs and an empty check list that reads like a pass (fixed in #240) — invisible, and not overridable by a label. That is why the skip lives in `gate` instead. `tests/ci_gate.rs` guards this, plus the rule that every job hangs off `gate`.

## Phases and hand-offs

A change goes through three phases, and **each phase is its own session**:

| Phase | Starts from | Ends with |
|---|---|---|
| Design | the request | a published **spec** |
| Implementation | `/goal implement <spec-url>` | an open PR whose spec tests pass |
| Follow-up | `/goal get PR #<n> green and through review` | a merged PR |

The next session starts from the hand-off's link, never from the conversation that produced it. `/handoff` closes a phase: it brings the spec or the PR description up to date and prints the next session's prompt. If a session has to carry on past a phase boundary, `/compact` there instead of waiting for auto-compact.

**The spec is the contract.** A plan that only describes the change leaves the implementation session to work the details out again, and gives the reviewer nothing to check the code against. The spec has these sections:

- **Goal / Non-goals**: one paragraph each.
- **Invariants and trust**: the rules the change rests on, stated as rules and **confirmed by the user** before any code. Cover every lifetime ("this lock is held for the request / for one use / until exit") and every new cross-component contract ("the provider reports X; who checks it: the engine at runtime, `heph validate`, or nobody?"). Give the alternatives and what each costs. On 2026-10-04, two PRs were half rebuilt because these were never asked. One built a per-revision lock rekey before hearing that locks must not outlive their use. In the other, the spec decided provider labels were "checked on every resolved spec", and the user reversed it after the PR was open: trust the provider and enforce with `heph validate`. A user's "lets do option 1" picks a mechanism. It does not confirm the invariant.
- **Decisions**: each settled decision with its reason. The implementation session does not reopen these.
- **Tests**: each test that defines done, with its layer (unit, `crates/e2e` or `crates/bin-e2e`; see `.claude/testing.md`) and what it proves. Implementation is done when these pass and `lint` exits 0. A test that is dropped or changed is recorded in the PR description.
  - When the spec enumerates cases ("six failure points", "three commands"), Tests maps **each case to a named test** in a table. A case with no row is how a whole stage shipped untested and came back as a review BLOCKER.
  - A heuristic (a parser, a text trimmer, a matcher) gets a table of **adversarial inputs** in Tests before it is written: the near-miss name (`//p:a` vs `//p:ab`), multi-line and empty input, input that went through a wrapper (a memoizer flattens error chains), non-ASCII and control bytes, input over any length cap. One heuristic was rewritten five times as reviewers found these one by one.
- **Accepted exemptions**: every place the change deliberately does *not* hold its own rule (a lock it does not take, a partial result it does write), each with a verdict from each design agent. An exemption written into Files as a side note was waved through at design and became a hermeticity BLOCKER at review.
- **Files**: the files and seams the change touches, and so which board triggers fire.
- **Board**: the verdicts from `/board`, one line per agent and round. A design is not done until it has a design-stage verdict from every agent it triggers. A list of agent names is not a verdict. `/handoff` refuses to close the design phase without them: the label spec above went to implementation unreviewed, although it named its own triggered agents.
- **Open**: the real remaining questions, each marked for the user or for implementation.

**One implementation context per stack layer.** A stack built and review-fixed in one context grows it past 400k, and every call re-sends all of it: one two-layer session ran 299 calls to a 736k context and 136M input tokens, 67% of it above 150k. Build each layer in its own context — a `general-purpose` agent with `isolation: "worktree"` per layer, continued with `SendMessage` for that layer's review fixes, or end the turn at the layer boundary and `/compact`. The cost is each layer re-reading its own code, which at a small context is a fraction of carrying the other layer.

**Rework goes to a fresh context too.** Two cases count as rework:
- a review-fix round after the first board;
- a redesign the user asks for once code exists.

Brief a fresh agent with the diff range, the finding or the user's words, and the decisions that still hold. Don't do the work in a context that carries the whole history. On 2026-10-04 two sessions made 209 calls above 300k, and both of them were doing this kind of work. One delegated its last fix to a fresh agent and that worked, about 80 calls later than it should have. The gate hook says when context passes 300k.

**An unrelated request is a new session.** Once the PR is pushed, a request that is not about it (another bug, a second small PR) starts a new session. Carried in the old one, it pays for the old one's history on every call.

**Independent changes run in parallel.** One design session can produce several specs. Each gets its own implementation session in its own workspace or worktree, branched off `master`. Stack only when a change can't compile or be reviewed without the one below it (see "Stacked PRs"). Wall-clock time is then the slowest PR, not the sum of all of them.

**Match the model to the job.** Lookups and triage don't need the strongest model: `Explore`, `/ci-triage` (which runs forked on Sonnet), and the `product-vision` and `feature-quality` consults at both stages use `model: "sonnet"`. `code-quality`, `hermeticity` and `compatibility` and the implementation itself inherit the session model.

**Match the effort to the phase, too.** `/effort xhigh` saved during design carries into implementation, and thinking is re-sent on every later call: one implementation session carried ~210k thinking tokens, ~35M input. Use `xhigh` for design and for review agents, `high` (or lower) for implementation, and raise it for the stretch that needs it — concurrency, locking, cache keys.

**Knowledge goes in the repo, not in memory.** Memory reaches one user's sessions, and only when it happens to be recalled. A trap about this repo belongs in the repo:
- a CI failure signature goes in `/ci-triage`;
- a tooling gotcha goes here or in the doc next to the code;
- a rule that can be checked becomes a test or a hook.

Memory is for the user's preferences.

## Session economy

Every turn re-sends the whole context, so a long session pays for its history on every call — and a turn over 400k tokens is slower as well as dearer. Across ten recent sessions, the four that ran past 300 turns took 92% of the tokens, at a median context of 300–470k; 64% of all input was context beyond the first 150k. The biggest saving is the phase split above.
- **Read narrowly.** `rg -n` to locate, then `sed -n 'A,Bp'` for the range — not `cat` of a whole source file or a whole `docs/*.md`. A 20KB dump is paid again on every later turn. A sweep across many files goes to an `Explore` agent, which returns the conclusion rather than the files.
- **Edit files with Edit/Write, not scripts.** A `python3 - <<EOF … s.replace(…)`, `sed -i` or `perl -pi` edit hides the diff from the user, leaves the harness's view of the file stale (every later touch re-sends it as "changed on disk"), and a heredoc of markdown mentioning `tst` trips the command gate. Several changes are several Edit calls in one message. Bash is for running things. The gate hook refuses these. The prose rule alone was broken 390 times in one session.
  - **One exception: a signature change the compiler verifies.** When adding an argument breaks N call sites and `cargo` lists every one, a `HEPH_SCRIPTED_EDIT=1 sed -i` limited to those `file:line`s is allowed — the compiler is the oracle, not the script. Do it as **one** command over the line numbers cargo printed. A second line-number `sed -i` after an edit that inserted lines hits the wrong lines.
  - Before you write a call site N times, ask whether it needs changing. Often the old signature can stay as a thin wrapper over a new `fn foo_with(.., X)`, and no caller changes. Follow it with `git diff --stat` and `git diff -U0` (so the user sees every change) and a `cargo clippy --keep-going` pass. Propagating one new argument through 48 test sites by hand took 47 Edits and 10.4M input tokens.
- **Send long output to a file.** Redirect build and test output into the scratchpad and grep what you need from the file, instead of letting a whole run into context. Judge the run by its exit code, not by the grep: rustfmt prints `Diff in`, not `error`, and `| tail` hides the status.
- **Don't poll.** CI: `/ci-triage` waits on the run and classifies each failure: a known flake (it reruns it), a failure inherited from a stacked base, or a real break. It reads the failed logs from a file, not into context. The primitive underneath is `gh run watch <run-id> --exit-status` with `run_in_background`. `gh pr checks` right after a push prints "no checks reported", which is not a pass. A local process: `run_in_background`, or Monitor with an until-loop.
- **Artifacts: one publish per round of feedback.** Collect the changes, edit the local file with `Edit`, publish once. Don't read back the published page — the local file is the source.
- **The shell is zsh with GNU coreutils from nix**, not bash or BSD: `stat -c` (not `-f`), no `mapfile`, `sed -i` takes no suffix argument. A directory under `~/.claude/projects/` starts with `-`, so it needs `--` or `./` before it reaches `ls`/`du`/`find`.

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
- **`/board design` and `/board review` run a consult.** The skill works out the triggered agents from the files touched, writes one brief, runs the consults in parallel, and records the verdicts in the spec's Board section.
- **Brief the agent; don't send it to rediscover the change.** Give it the diff range or artifact link, the specific question, and the files that matter. An agent told only "review this" re-reads the codebase from scratch: one design session spent 124M tokens on 36 consults, nearly twice its own 67M. Consult the always-consult three plus only the agents whose trigger fired. For a follow-up round, continue the same agent with `SendMessage` rather than spawning a fresh one.
- A **BLOCKER** from `feature-quality`, `code-quality`, `hermeticity`, or `compatibility` is fixed, or explicitly overruled with a stated reason, before the commit.
- **NOT HERMETIC** and **BREAKING** are never silently accepted — either fix, or record the decision in the commit body.
- A **RETHINK** / **DON'T BUILD** from `product-vision` goes back to the user, not around them.
- **Per-platform behavior is the user's decision.** The supported set is `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin` — no BSD, no Windows, no 32-bit — and features work uniformly across all three by default, on the OS axis (Linux vs macOS) and the arch axis (x86_64 vs aarch64) alike. A divergence may be the right answer — but it is never settled by an agent or by the implementation. Flag it (what differs, on which target, what each option costs) and put the call to the user before writing the code. CI runs the suite natively on all three supported targets, so a green CI does cover an arch-conditional change — but the `linux/arm64` *release* binary is cross-compiled while its test job builds natively, so a toolchain-specific break there is still uncovered.
- **Adding a dependency is allowed** when it gets the job done reliably. A maintained crate beats a fragile hand-rolled version. What still gets flagged: duplicating a crate already in the tree, a second copy of an ecosystem (async runtime, TLS, HTTP client, allocator), hot-path or startup cost, and support limited to one OS or one arch. Adding a dependency also re-resolves the **whole** lockfile.
  - Once, adding 40 crates bumped tokio and 200 other packages, and it broke an unrelated engine test on macOS only. It looked like a flake.
  - Check `git diff master -- Cargo.lock | grep -c '^-version'`. If existing pins moved, run `git checkout master -- Cargo.lock && cargo check --workspace` so that only what the new crates need changes.

@.claude/rust.md
@.claude/testing.md
@.claude/architecture.md