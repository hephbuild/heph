{ pkgs, lib, config, inputs, ... }:

let
  # kache — the `RUSTC_WRAPPER` (see `env.RUSTC_WRAPPER` below). Not in nixpkgs,
  # and the upstream flake builds it from source against a pinned toolchain,
  # which would land in the devenv shell's critical path on every cold CI
  # runner. The published release binaries are self-contained (static musl on
  # Linux, signed Mach-O on darwin), so fetch those instead: ~2s and no compile.
  #
  # Bumping: change `version`, then re-derive each `hash` — upstream publishes a
  # `<asset>.sha256` next to every asset, and
  #   nix hash convert --hash-algo sha256 --to sri <hex>
  # turns that hex into the SRI form below. All four are verified by Nix at
  # fetch time, so a wrong hash fails loudly rather than silently installing
  # something else.
  kacheVersion = "0.13.0";
  kacheAssets = {
    aarch64-darwin = {
      target = "aarch64-apple-darwin";
      hash = "sha256-8/TXz+ziDSUfh0Da4iR3fQlqXtKxYIscrplumPYBijE=";
    };
    x86_64-darwin = {
      target = "x86_64-apple-darwin";
      hash = "sha256-waUdA52DT2qtusGhZrlUTVMgs8qMsEhSD0XH1nqsG7U=";
    };
    x86_64-linux = {
      target = "x86_64-unknown-linux-musl";
      hash = "sha256-MK7e1NxuYgxACqOq96sWPclccDoPPdtNC6VsUfI/C9A=";
    };
    aarch64-linux = {
      target = "aarch64-unknown-linux-musl";
      hash = "sha256-th3j3mqauyGjdfqcZRPUe7jPc5H07rJMW+KXJzi4POM=";
    };
  };
  kacheAsset =
    kacheAssets.${pkgs.stdenv.hostPlatform.system}
      or (throw "kache: no prebuilt binary for ${pkgs.stdenv.hostPlatform.system}");
  kache = pkgs.stdenvNoCC.mkDerivation {
    pname = "kache";
    version = kacheVersion;
    src = pkgs.fetchurl {
      url = "https://github.com/kunobi-ninja/kache/releases/download/v${kacheVersion}/kache-${kacheAsset.target}.tar.gz";
      inherit (kacheAsset) hash;
    };
    # The tarball is a single `kache` at the root — no directory to strip.
    sourceRoot = ".";
    dontBuild = true;
    # Static musl / already-signed Mach-O. patchelf would only break the former
    # (there is no interpreter to rewrite) and invalidate the latter's signature.
    dontPatchELF = true;
    dontStrip = true;
    installPhase = "install -Dm755 kache $out/bin/kache";
  };

  binLocation = "$HOME/.local/bin/heph3";
  qualityCrates = "-p heph -p e2e -p bin-e2e -p testkit -p plugingo-e2e -p htspec-derive -p core -p config -p walk -p proc -p execrunner -p model -p sandboxfuse -p plugin -p plugin-abi -p plugin-sdk -p plugin-stabby -p plugin-go-cdylib -p builtins -p plugin-buildfile -p driver-support -p driver-bridge -p plugin-exec -p plugin-nix -p plugin-devenv -p plugin-devenv-cdylib -p plugin-http -p plugin-oci -p plugin-query -p plugin-go -p plugin-gha -p plugin-gha-cdylib -p plugin-oci-cdylib -p telemetry -p tui -p lock -p selfupdate -p engine -p xstarlark-fmt -p bench-corpus -p bench";
in
{
  # https://devenv.sh/basics/

  # https://devenv.sh/packages/
  packages = [
    pkgs.git
    # The Go toolchain the go-plugin tests build against. Everything that calls
    # `require_go!` runs `go` from PATH (`gotool = "host"`), so without this the
    # whole suite depends on whatever the machine happens to have — and CI's
    # macOS runner has none, which silently skipped every Go test there: 463
    # `plugin-go` unit tests finished in 0.55s instead of 62s, and all 33
    # `plugingo-e2e` tests in 0.25s instead of ~10min. Pinning it here fixes
    # that for CI and local runs at once, and `gen-go-large` gets a `go` too.
    #
    # A `gotool = "<version>"` workspace downloads its own hermetic SDK and is
    # unaffected by this; only the `host` toolchain reads it.
    pkgs.go
    pkgs.buf
    pkgs.protoc-gen-prost
    pkgs.protoc-gen-prost-serde
    pkgs.protoc-gen-prost-crate
    pkgs.zig
    pkgs.cargo-zigbuild
    pkgs.tokio-console
    kache
    # `rust-objcopy`/`rust-strip` (wraps the `llvm-tools` component's
    # llvm-objcopy, below) — used by `scripts/patch-slot.sh`'s CI caller to
    # derive the "std" release flavour's stripped binary. LLVM-based and
    # target-agnostic, unlike host binutils `strip`: the Linux arm64 leg
    # cross-compiles on an amd64 runner (see the zigbuild comment in
    # `.github/workflows/heph.yml`), and a native `strip` typically can't
    # touch a foreign-arch ELF. (`zig objcopy` was tried first — it's already
    # in this shell for zigbuild — but its `--strip-all` hits unimplemented
    # code paths on real release-profile output; cargo-binutils' llvm-objcopy
    # is the mature tool for this.)
    pkgs.cargo-binutils
    # Coverage (`scripts.cov`). grcov turns the `.profraw` that rustc's own
    # `-Cinstrument-coverage` emits into lcov; the `llvm-profdata` it shells out
    # to comes from the `llvm-tools` component above, *not* from here — see the
    # `cov` script for why the two must not be mixed.
    pkgs.grcov
    # `scripts/patch-slot.sh` stamps the version/flavour slots with it. Pinned
    # here rather than taken from the host: CI ran it on whatever python the
    # runner image happened to ship, and the local `e2e` path now runs the same
    # patch (see `scripts.e2e`), where a machine without an interpreter — a Mac
    # with no Xcode CLT, say — would otherwise fail to stage artifacts at all.
    pkgs.python3
    # pkg-config + libfuse for the `fuse-sandbox` feature.
    # - Linux: `fuse3` ships headers/pc files fuser links against.
    # - macOS: `macfuse-stubs` provides the build-time `osxfuse.pc` per
    #   fuser's README (https://github.com/cberner/fuser). The kext
    #   itself still needs the macFUSE installer at runtime.
    pkgs.pkg-config
  ] ++ lib.optionals pkgs.stdenv.isDarwin [
    pkgs.samply
    pkgs.macfuse-stubs
  ] ++ lib.optionals pkgs.stdenv.isLinux [
    pkgs.fuse3
  ];

  # Route every rustc invocation through kache (local + CI, since CI runs inside
  # this shell). Replaced sccache, which cannot cache "crates that invoke the
  # system linker" — i.e. `bin`, `dylib`, `cdylib` and `proc-macro`. That is
  # exactly this workspace's expensive tail: the `heph` binary, the three plugin
  # cdylibs, every proc-macro, and every test harness. kache caches all of them.
  #
  # No remote is configured here, so a local shell is a local-disk cache and
  # needs no daemon. CI points the same wrapper at R2 via `KACHE_S3_*` (see
  # `.github/actions/setup-nix`). To share CI's cache locally, export those same
  # vars and run `kache daemon start` — the remote is inert without the daemon.
  env.RUSTC_WRAPPER = "kache";

  # Cache linked `bin` and `--test` executables on macOS too, not just Linux.
  #
  # kache defaults this on for Linux and off for macOS: a Mach-O binary carries
  # only a *debug map* (`N_OSO` entries naming each `.o` by path + mtime) rather
  # than embedded DWARF, so a restored mac binary points at object files that no
  # longer match and LLDB drops to function names without file:line. Linux
  # embeds DWARF in the binary and restores losslessly.
  #
  # Turned on anyway, uniformly, because the release artifacts already have no
  # file:line on macOS to lose: cargo overrides rustc's macOS default to
  # `split-debuginfo = "unpacked"` for any profile with debug info (including
  # `[profile.release]`, which sets `debug = "line-tables-only"`), so the shipped
  # mac binary references `.o` files left behind on a CI runner. Fixing that
  # needs a `.dSYM` shipped as a release asset — tracked separately, not here.
  # The only live cost is attaching a debugger to a locally cache-restored mac
  # binary; touch the crate and rebuild to get a freshly linked one.
  env.KACHE_CACHE_EXECUTABLES = "1";

  # https://devenv.sh/languages/
   languages.rust = {
     enable = true;
     channel = "stable";
     # `llvm-tools`: ships llvm-objcopy/llvm-strip in the sysroot, which
     # `cargo-binutils` (above) wraps as `rust-objcopy`/`rust-strip`.
     components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-analyzer" "llvm-tools" ];
     targets = [ "x86_64-apple-darwin" "aarch64-apple-darwin" ]
       ++ lib.optionals pkgs.stdenv.isLinux [ "x86_64-unknown-linux-gnu" "aarch64-unknown-linux-gnu" ];
   };

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/scripts/
  scripts.gen-proto.exec = "buf generate";
  scripts.gen.exec = "rm -rf gen && gen-proto";
  scripts.gen-go-large.exec = ''
    rm -rf $DEVENV_ROOT/example/go/large
    cd $DEVENV_ROOT/tools/gorepogen
    go run . -seed 42 -out $DEVENV_ROOT/example/go/large -module example.com/large -pkgs 500 -max-depth 7
    cd $DEVENV_ROOT/example/go/large && go mod tidy
  '';
  # Set up the example workspace end to end: regenerate the large go repo and
  # install the go plugin (cdylib + manifest) into ~/.heph/plugins/go via
  # `install-go-plugin`. example/.hephconfig2 loads it in-process behind the
  # stable ABI via `path: ~/.heph/plugins/go/heph-go-plugin.json` (native speed —
  # see ai-docs/PERFORMANCE.md).
  scripts.gen-example.exec = ''
    gen
    gen-go-large
    install-go-plugin
    # The example workspace's `execrunner` package needs both: a
    # `devenv_runner` for the build environment and an `oci_runner` for the
    # runtime one. Neither is compiled into the CLI.
    install-devenv-plugin
    install-oci-plugin
  '';
  # Three clippy passes — default features, `--all-features`, and
  # `--no-default-features` — then fmt-check all hand-written crates
  # (qualityCrates; generated gen/proto is excluded). What each pass is actually
  # worth is measured below rather than assumed from its name: the
  # `--all-features` one covers almost nothing here, and the
  # `--no-default-features` one only works with the exclusions it carries.
  #
  # `--workspace --all-targets` is load-bearing. Do NOT shorten it. Both flags
  # were verified by injecting a deliberate error and re-running, not assumed
  # from the flag name:
  #
  #   - Without `--workspace`: the repo root is itself a package (`heph`), so
  #     cargo's default selection is that package and *its dependency graph*.
  #     Two things fall outside it. Every member that the root binary does not
  #     depend on is never compiled at all — `plugin-go`, `plugin-gha`, the
  #     cdylibs, `plugin-abi`/`sdk`/`stabby`, `testkit`, and the `e2e` crates.
  #     And no member's **test** targets are built, in the graph or out. (A
  #     member lib that *is* in the graph does get linted, because cargo passes
  #     that package's `[lints]` to rustc regardless of primary status — so the
  #     hole is "not compiled" and "tests not compiled", not "compiled without
  #     lints".)
  #   - Without `--all-targets`: `#[cfg(test)]` modules, `tests/`, `benches/`
  #     and `examples/` are skipped. Most of this repo's test code lives in
  #     `#[cfg(test)] mod tests` inside member crates.
  #   - Without `--workspace` the `--all-features` pass was a 0.30s no-op: the
  #     root package's sole feature (`fuse-sandbox`) is already default, and the
  #     feature-gated code it promises to cover lives in members that were not
  #     selected. Measured honestly, it is *still* close to a no-op with
  #     `--workspace` (2.31s, zero units recompiled after the default pass):
  #     only four members declare features, and `--workspace` already unifies
  #     every one of them on — the cdylibs pull `plugin-sdk/stabby` →
  #     `plugin-stabby/host` → `plugin-abi`, and the root's default pulls
  #     `sandboxfuse/fuse-sandbox`. It is kept as cheap insurance against a
  #     future feature nothing else references, not because it covers anything
  #     today. The pass that does add coverage is `--no-default-features`.
  #
  # Together those hid 323 clippy errors, and let the gate pass on a tree that
  # did not compile: a trait impl missing a method inside `crates/engine`'s
  # `mod tests` is invisible to a run that never builds that target, so `Lint`
  # went green while every `Test` job went red on the same commit.
  #
  # A bare `cargo clippy` here is green while CI is red, and vice versa. If you
  # want a faster local loop, narrow with `-p <crate>` — never by dropping
  # `--workspace`.
  #
  # `tests/lint_gate.rs` asserts these flags are still here, and that every
  # workspace member inherits `[workspace.lints]` — a member that does not is
  # linted with stock clippy only, which is the same silent hole one level down.
  #
  # The third pass covers `fuse-sandbox` **off**, and its selection is the
  # subtle one. `--workspace --no-default-features` looks like it turns the
  # feature off and does not: `crates/e2e`, `crates/plugingo-e2e` and
  # `crates/testkit` depend on the root `heph` package with default features on,
  # and cargo unifies features across the whole selection, so that edge switches
  # `fuse-sandbox` straight back on. Verified, not reasoned: with an
  # `indexing_slicing` error injected into `crates/sandboxfuse/src/stub.rs` — the
  # file that is compiled *only* when the feature is off — the naive invocation
  # exits 0 and the excluded one exits 101.
  #
  # Because that is invisible when it regresses (a new member with
  # `heph = { path = ".." }` silently re-enables it, and the pass keeps
  # reporting green while linting the wrong arm), the script *asserts* the
  # feature is off before linting rather than trusting the flag. `cargo tree -i`
  # exits non-zero when the package is absent from the resolved graph, which is
  # the state we want.
  #
  # Remaining gaps, so nobody has to rediscover them:
  #   - `--all-targets` does not include doctests (10 blocks).
  #   - `cargo test --no-default-features` is not run — the feature-off arm is
  #     compiled and linted, never executed. `crates/sandboxfuse`'s
  #     `fuse_sandbox_is_a_default_feature` asserts the feature *is* on, so a
  #     test leg would need that test `#[cfg]`-split first.
  #   - CI lints `linux/amd64` and `darwin/arm64`, so the OS axis is covered but
  #     the arch axis is confounded: `aarch64` is only ever linted together with
  #     macOS, and there is no `Lint linux/arm64`. An `#[expect(clippy::…)]`
  #     that is unfulfilled only on `aarch64-unknown-linux-gnu` would be
  #     invisible — plain rustc reports `unfulfilled_lint_expectations` for its
  #     own lints, but silently ignores an unfulfilled expectation on a *tool*
  #     lint it does not know, so `Test linux/arm64` cannot stand in for a lint
  #     job. Either keep lint exemptions free of `target_arch` conditions, or
  #     add the third leg.
  #
  # The flags are written out literally in both scripts rather than factored
  # into a shared Nix variable on purpose: `tests/lint_gate.rs` guards them by
  # reading this file, and a guard that reads back an interpolation hole rather
  # than the flags guards nothing.
  scripts.lint.exec = ''
    set -euo pipefail
    echo '> clippy'
    cargo clippy --workspace --all-targets --locked -- -D warnings
    echo '> clippy --all-features'
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    # Assert the feature is genuinely off before linting it. A *successful*
    # `cargo tree -i fuser` means `fuser` is still in the resolved graph, i.e.
    # the pass below would lint the fuse-sandbox=on arm while announcing the
    # opposite. Absence is specifically "did not match any packages" — matching
    # on the exit code alone would read *any* cargo failure (an ambiguous
    # `fuser` after a transitive version bump, a resolver error) as "feature
    # off" and wave the wrong arm through, which is the silent-green failure
    # this whole gate exists to remove.
    echo '> checking --no-default-features really disables fuse-sandbox'
    if fuser_tree=$(cargo tree --workspace --exclude e2e --exclude plugingo-e2e --exclude testkit --exclude bench --no-default-features --locked -i fuser 2>&1); then
      echo "error: --no-default-features left 'fuser' in the dependency graph, so the pass below would lint the fuse-sandbox=on arm and the feature-off code is still covered by nothing." >&2
      echo "       Some selected package pulls the root 'heph' package (or 'sandboxfuse') with default features on; exclude it here and in 'fix'." >&2
      printf '%s\n' "$fuser_tree" >&2
      exit 1
    elif ! printf '%s' "$fuser_tree" | grep -q 'did not match any packages'; then
      echo "error: cargo tree failed for a reason other than 'fuser' being absent, so this check proved nothing:" >&2
      printf '%s\n' "$fuser_tree" >&2
      exit 1
    fi
    # Positive control. Without it, `fuser` leaving the graph for an unrelated
    # reason — renamed, or the FUSE backend swapped — makes the check above pass
    # forever while proving nothing, because "absent when off" and "absent
    # always" look identical from one direction.
    if ! cargo tree --workspace --locked -i fuser >/dev/null 2>&1; then
      echo "error: 'fuser' is not in the graph even with fuse-sandbox on, so the check above cannot tell 'feature off' from 'package gone'. Point this probe at whatever the feature now pulls in." >&2
      exit 1
    fi
    echo '> clippy --no-default-features'
    cargo clippy --workspace --exclude e2e --exclude plugingo-e2e --exclude testkit --exclude bench --all-targets --no-default-features --locked -- -D warnings
    echo '> fmt'
    cargo fmt --check ${qualityCrates}
  '';
  # The write half of `lint`, and it must select the same code — a `fix` that
  # only reaches the root package leaves the member lints `lint` reports with
  # no automated fix at all. `clippy --fix` rather than `cargo fix`: it applies
  # clippy's machine-applicable suggestions as well as rustc's, and clippy's are
  # what `lint` fails on. The `--no-default-features` line carries the same
  # exclusions for the same reason as in `lint`; without it CI reports a lint in
  # `stub.rs` that `fix` cannot reach. `tests/lint_gate.rs` asserts the two
  # scripts select the same code.
  scripts.fix.exec = ''
    set -euo pipefail
    cargo clippy --fix --workspace --all-targets --allow-dirty --allow-staged
    cargo clippy --fix --workspace --exclude e2e --exclude plugingo-e2e --exclude testkit --exclude bench --all-targets --no-default-features --allow-dirty --allow-staged
    cargo fmt ${qualityCrates}
  '';
  # Test everything. The default pass covers all crates with default features; the
  # targeted passes exercise the feature-gated transport code, off by default:
  # the stabby host loader/adapters (plugin-stabby `host`) and the stabby guest
  # serving (plugin-sdk `stabby` — the SDK is transport-agnostic by default).
  # `bin-e2e` is excluded on purpose: it drives *shipped artifacts*, not this
  # source tree, and has no meaning without a staged dist. Run it with `e2e`.
  scripts.tst.exec = "cargo test --locked --workspace --exclude bin-e2e && cargo test --locked -p plugin-stabby --features host && cargo test --locked -p plugin-sdk --features stabby";

  # Line coverage. ONE entrypoint, identical locally and in CI — the `coverage`
  # job in `.github/workflows/heph.yml` runs exactly this and then uploads the
  # file it wrote. Same contract as `e2e`.
  #
  #   cov                  # the gate suite; writes coverage/{lcov.info,summary.json}
  #   cov -p engine        # narrow local loop — args go straight to `cargo test`
  #
  # Reads out of `coverage/` at the repo root: `lcov.info` (filtered — what
  # Codecov reads, and what editor gutters should point at), `lcov.raw.info`
  # (grcov's own output, before `#[cfg(test)]` stripping), `summary.json`
  # (sorted keys, no timestamps) for agents and for diffing two runs,
  # `test.log`, and a worst-covered-first table on stdout for humans. The
  # `.profraw` themselves live under `target/coverage/` — bulky, already
  # gitignored, and deleted as soon as grcov has read them. `grcov -t html` over
  # them produces a browsable report if you want one before that happens; it is
  # not generated by default because it is a second full parse of every profile
  # for something CI never reads.
  #
  # `#[cfg(test)]` modules are stripped from the report by
  # `scripts/coverage-report.py`. 39% of this tree's Rust lines are inside one,
  # source-based coverage instruments them along with the code they test, and no
  # path-based exclusion can reach them because they live inside production
  # files. Left in they do not merely inflate the headline: `.claude/testing.md`
  # requires every change to ship with a test, so every PR is production code
  # plus a test module, and patch coverage — the number a reviewer actually
  # reads — comes out flattering in proportion to how much test code was added.
  #
  # WHY grcov, over the two other candidates:
  #   - kcov sets DWARF breakpoints on line entries. Linux-only — it cannot run
  #     on `aarch64-apple-darwin`, one of the three supported targets — and its
  #     accuracy degrades exactly where this workspace lives: monomorphised
  #     generics, inlining, and async state machines.
  #   - cargo-tarpaulin's default engine is ptrace, so also Linux-only, and
  #     ptrace is the worst-placed mechanism for a program whose interesting
  #     seams are spawned subprocesses and `dlopen`'d plugin cdylibs.
  #   - grcov consumes the `.profraw` that *rustc's own* `-Cinstrument-coverage`
  #     emits. Nothing platform-specific, so it runs on all three targets; it
  #     counts regions rather than line breakpoints, so generics and async do
  #     not confuse it; and because the counters live in the instrumented binary
  #     rather than in a tracer, an instrumented child process or cdylib writes
  #     its own `.profraw` and is counted with everything else. `llvm-tools` is
  #     already a toolchain component here (for `rust-objcopy`) and carries the
  #     `llvm-profdata` grcov needs.
  #
  # Not measured, and deliberately: doctests still *run* under `cov` but are not
  # instrumented on stable, so they contribute nothing to the report; and `bin-e2e` is excluded for the
  # same reason `tst` excludes it — it drives shipped artifacts, not this tree —
  # so the loader, the TUI and the exit-code paths report as uncovered despite
  # being among the most carefully tested things in the repo. The four
  # `*-cdylib` crates are compiled but never `dlopen`ed here for the same
  # reason, which is why `codecov.yml` excludes them rather than letting a
  # permanent 0% sit in the denominator.
  #
  # Nothing downstream of this script can fail. `codecov.yml` is `informational`
  # and the job is not in `release`'s `needs:`, so a report that measured
  # nothing would publish as a number and read as a drop. Every check that could
  # notice therefore lives here, and exits non-zero: no profiles, no tests, a
  # profile written outside `$LLVM_PROFILE_FILE`, a profile grcov skipped, a
  # report under the size floors, or a named file that must have been executed
  # and was not.
  #
  # One caveat on "identical locally and in CI": grcov reads coverage mappings
  # from every object under `--binary-path`, and a CI runner's target dir holds
  # only this build's outputs while a local one is never pruned. A test binary
  # left over from an earlier commit therefore contributes its lines locally, as
  # uncovered. `cargo clean` if a local number looks unaccountably low.
  #
  # No `CARGO_TARGET_DIR` override, per the rule in CLAUDE.md. Instrumented and
  # plain builds do invalidate each other's fingerprints in the shared
  # `target/`, but kache is content-keyed on (source, args, compiler), so
  # alternating `tst` and `cov` restores from the local store rather than
  # recompiling — which is the same argument that removed the shared target dir
  # in the first place.
  scripts.cov.exec = ''
    set -euo pipefail

    root="$DEVENV_ROOT"

    # Everything below is anchored on `$DEVENV_ROOT` — the sources grcov reads,
    # the target dir, where the profiles land — while `cargo` compiles whatever
    # `$PWD` happens to be. A shell started in one worktree and used in another
    # would measure B's binaries against A's sources and A's leftover profiles,
    # meet the floors, and print a plausible number describing the wrong tree.
    # That is the exact silent-wrong-report this whole script exists to prevent,
    # so pin the two together rather than checking for it.
    cd "$root"

    out="$root/coverage"
    host="$(rustc -vV | sed -n 's/^host: //p')"

    # `.profraw` lives under `target/`, not in the source tree: it is the bulky
    # half (one file per test process), it is already gitignored, and `gen`'s
    # repo artifact does not carry `target/`. Only the small reports land in
    # `coverage/`, at a fixed path the workflow can name without a devenv shell.
    profraw="$(target-dir)/coverage/profraw"

    # Stale `.profraw` would be merged straight into this report. They are keyed
    # by pid and binary hash, never by run, so nothing about an old one looks
    # old — it just quietly moves the number.
    rm -rf "$out" "$profraw"
    mkdir -p "$out" "$profraw"

    # `RUSTFLAGS` in the environment *replaces* the flags configured in
    # `.cargo/config.toml`; cargo takes its extra flags from exactly one source
    # and never merges them. This repo already knows that trap from the other
    # side — `build.rs`'s `frame_pointers()` exists because an env `RUSTFLAGS`
    # silently drops `-Cforce-frame-pointers=yes` and turns `--pprof-cpu` into a
    # random-stack generator. Carrying the real list over rather than hardcoding
    # a copy is the whole point: a copy is the same bug one refactor later.
    base_rustflags="$(python3 - "$root/.cargo/config.toml" <<'PY'
    import sys, tomllib

    with open(sys.argv[1], "rb") as f:
        config = tomllib.load(f)

    # `target.<triple>.rustflags` sits *above* `build.rustflags` in cargo's
    # precedence, so its presence would make the list read below inert for real
    # builds while this script kept faithfully carrying it over -- blocker #1
    # again, one refactor later and invisible.
    targeted = sorted(
        name
        for name, table in config.get("target", {}).items()
        if isinstance(table, dict) and "rustflags" in table
    )
    if targeted:
        sys.exit(
            "cov: .cargo/config.toml sets target.<triple>.rustflags for "
            + ", ".join(targeted)
            + ", which supersedes build.rustflags. Teach this script to read "
            "them before the instrumented build starts losing flags silently."
        )

    flags = config.get("build", {}).get("rustflags")
    if not flags:
        sys.exit(
            "cov: .cargo/config.toml has no build.rustflags to carry over. If "
            "they moved, point this at the new home -- appending to nothing "
            "silently builds the instrumented tree without them."
        )
    print(" ".join(flags))
    PY
    )"

    # `-Cdebuginfo=0` is not an optimisation, it is what makes the job fit.
    # Source-based coverage carries its own `__llvm_covmap`/`__llvm_covfun`
    # mapping and reads no DWARF at all (unlike kcov/gcov, which are built on
    # it), while the dev profile compiles all ~680 locked packages at full
    # debuginfo — and `Test linux/amd64` already needs a "free disk space" step
    # to survive `tst` uninstrumented. The cost is that a panic *in this job*
    # has no file:line; `test` runs the same suite with full debug info and is
    # the leg that gates, so nothing is lost from CI overall.
    export RUSTFLAGS="$base_rustflags -Cinstrument-coverage -Cdebuginfo=0"

    # An explicit target is what splits host units from target units. Without
    # it cargo builds build scripts and proc-macros with the same RUSTFLAGS, so
    # `htspec-derive` becomes an *instrumented* proc-macro that rustc dlopens —
    # and every rustc invocation in the workspace then writes its own
    # `.profraw`, burying the test profiles under thousands of files and
    # reporting build-script lines as covered code. `CARGO_BUILD_TARGET` rather
    # than `--target` so `tst` below stays the single source of truth for what
    # the suite selects.
    export CARGO_BUILD_TARGET="$host"

    # Absolute, because tests change directory constantly (every `TempDir`
    # fixture does) and a relative pattern scatters `.profraw` wherever a test
    # happened to be standing. `%p` (the pid) is what keeps concurrently running
    # test binaries apart — `cargo test` is parallel here, and one shared file
    # is a clobber, not a merge. `%m` (the binary's signature) is not redundant
    # beside it: pids are reused within a run this long, and two different
    # binaries landing on one name is a corrupt profile rather than a lost one.
    export LLVM_PROFILE_FILE="$profraw/%p-%m.profraw"

    # Incremental objects are not instrumented consistently across a partial
    # rebuild, which shows up as coverage moving while the code did not.
    export CARGO_INCREMENTAL=0

    log="$out/test.log"

    # The floors below only make sense for the whole suite. A narrow local loop
    # (`cov -p engine`) legitimately produces a small report, and a floor that
    # fires on it would teach people to ignore the one check that matters.
    floors=()

    # A failing suite must not cost you the report. Under `pipefail` a single
    # flaky test would otherwise abort `cov` before grcov ever runs: no lcov, no
    # step summary, and a second red job whose log says nothing the first one
    # did not. Collect the status, report anyway, and fail at the end.
    suite_rc=0
    if [ "$#" -gt 0 ]; then
      cargo test --locked "''${@}" 2>&1 | tee "$log" || suite_rc=$?
    else
      floors=(
        --min-files 100
        --min-lines 10000
        --require-covered crates/core/src/hmemoizer/mod.rs
      )
      # `tst`, not a copy of its package list. The report has to describe the
      # suite that actually gates this repo, and a second selection here would
      # drift from that one silently — the number would just stop covering a
      # crate, with nothing to see. It also keeps the two feature-gated passes
      # (`plugin-stabby --features host`, `plugin-sdk --features stabby`), whose
      # absence would read as "the stabby transport is untested".
      tst 2>&1 | tee "$log" || suite_rc=$?
    fi

    if [ "$suite_rc" -ne 0 ]; then
      echo "cov: the test suite failed (exit $suite_rc). The report below covers" >&2
      echo "     the run that failed, so it is partial; the floors are skipped." >&2
      # Otherwise a partial suite trips "the report is too small" and buries the
      # actual cause under a message about collection.
      floors=()
    fi

    profraw_count="$(find "$profraw" -name '*.profraw' -type f | wc -l | tr -d ' ')"
    if [ "$profraw_count" -eq 0 ]; then
      if [ "$suite_rc" -ne 0 ]; then
        echo "cov: the suite failed before any profile was written." >&2
        exit "$suite_rc"
      fi
      echo "cov: no .profraw was written under $profraw." >&2
      echo "     The instrumented binaries never ran, or LLVM_PROFILE_FILE did not" >&2
      echo "     reach them. Coverage was NOT measured -- this is not 0% coverage." >&2
      exit 1
    fi

    # A zero-length profile is the one corruption that defeats everything else.
    # `llvm-profdata` skips it in silence -- no warning, exit 0 -- so grcov
    # succeeds, the stderr scan below finds nothing, and the floors pass on the
    # strength of the profiles that did survive. The result is a smaller,
    # entirely plausible number published as a drop. It is not hypothetical on a
    # runner that already needs a "free disk space" step: a process killed
    # between LLVM creating the file and writing it (ENOSPC, OOM) leaves exactly
    # this.
    empty="$(find "$profraw" -name '*.profraw' -type f -empty -print | head -n 5)" || true
    if [ -n "$empty" ]; then
      echo "cov: some profiles are zero-length, so their counters are lost:" >&2
      printf '%s\n' "$empty" | sed 's/^/  /' >&2
      echo "     A test process died before writing its profile (ENOSPC or OOM are" >&2
      echo "     the usual causes). llvm-profdata skips these silently, so the" >&2
      echo "     report would be smaller than the truth rather than wrong-looking." >&2
      exit 1
    fi

    # A child that loses `LLVM_PROFILE_FILE` does not fail: LLVM falls back to
    # `default_<sig>_<pid>.profraw` in the child's cwd. Today the only
    # instrumented children inherit the env, but sandboxed ones are `env_clear`ed
    # (`crates/proc/src/proc_exec/imp_*.rs`) and the execrunner session path
    # re-executes `current_exe` inside the sandbox — so the day that path runs
    # under coverage, a stray profile lands in a sandbox where it is both a lost
    # counter and an undeclared output. `$TMPDIR` as well as the repo because
    # that is where the sandboxes actually are: every test workspace comes from
    # `tempfile::tempdir()` (`crates/testkit`), which is `/tmp` on Linux and
    # `/var/folders/…` on macOS — searching only `$root` would look diligent and
    # cover none of the case this describes.
    #
    # `|| true` is load-bearing. `head -n 5` exits as soon as it has five lines,
    # `find` takes SIGPIPE, and `pipefail` propagates 141 — which `set -e` turns
    # into the script dying with no message at all, in precisely the scenario
    # this check exists to report.
    strays="$(
      find "$root" "''${TMPDIR:-/tmp}" \
        -name .git -prune -o \
        -name 'default_*.profraw' -type f -print 2>/dev/null | head -n 5
    )" || true
    if [ -n "$strays" ]; then
      echo "cov: profiles were written outside \$LLVM_PROFILE_FILE:" >&2
      printf '%s\n' "$strays" | sed 's/^/  /' >&2
      echo "     Something spawned an instrumented child with a cleared environment." >&2
      echo "     Those counters are missing from the report, and inside a sandbox they" >&2
      echo "     are an undeclared output." >&2
      exit 1
    fi

    # A suite that ran zero tests exits 0 — the `e2e tui_pty` trap in a new
    # place. grcov would then happily report the coverage of process startup and
    # nothing about the run would look wrong.
    tests_run="$(awk '/^running [0-9]+ tests?$/ { n += $2 } END { print n + 0 }' "$log")"
    if [ "$tests_run" -eq 0 ]; then
      echo "cov: the run executed 0 tests, so this report measures nothing." >&2
      echo "     A cargo test filter that matches no test still exits 0." >&2
      exit 1
    fi

    # rustc's own llvm-profdata, never the host's: the `.profraw` format is tied
    # to the LLVM the compiler was built with, and a mismatched profdata reads a
    # subset of the profiles rather than erroring. Checked rather than assumed —
    # drop `llvm-tools` from `languages.rust.components` and grcov would be
    # handed a `--llvm-path` pointing at nothing, which is the same silent-green
    # by a different route.
    llvm_bin="$(rustc --print sysroot)/lib/rustlib/$host/bin"
    if [ ! -x "$llvm_bin/llvm-profdata" ]; then
      echo "cov: no llvm-profdata at $llvm_bin." >&2
      echo "     The 'llvm-tools' component is what ships it; see" >&2
      echo "     languages.rust.components in devenv.nix. Falling back to a nixpkgs" >&2
      echo "     LLVM would read a subset of the profiles rather than fail." >&2
      exit 1
    fi

    # `--ignore` here covers only what is not this project's source: registry and
    # nix store paths that leak in through inlined generics, and `gen/` (written
    # by buf, hand-edited by nobody). Test *scaffolding* that is this project's
    # source — crates/e2e, testkit, the bench harnesses — is excluded in
    # `codecov.yml` instead, so each exclusion has exactly one home. It stays in
    # the local table on purpose: knowing the e2e harness is itself half dead
    # code is useful here and noise on a PR.
    #
    # grcov's reaction to a profile it cannot parse is a warning on stderr and
    # exit 0, which is this feature's most likely silent-green: a smaller,
    # entirely plausible number. Capture stderr, show it, and treat those
    # warnings as fatal. The keyword list covers what llvm-profdata actually
    # says — `truncated profile data` is its wording for a partially written
    # file and matches none of the more obvious words.
    raw="$out/lcov.raw.info"
    grcov_err="$out/grcov.log"
    if ! grcov "$profraw" \
      --binary-path "$(target-dir)/$host/debug" \
      --llvm-path "$llvm_bin" \
      --source-dir "$root" \
      --output-types lcov \
      --output-path "$raw" \
      --ignore-not-existing \
      --ignore '/*' \
      --ignore '../*' \
      --ignore 'gen/*' \
      2>"$grcov_err"
    then
      cat "$grcov_err" >&2
      echo "cov: grcov failed to produce a report." >&2
      exit 1
    fi
    if [ -s "$grcov_err" ]; then
      cat "$grcov_err" >&2
    fi

    if grep -qiE 'malformed|unsupported|corrupt|truncated|not a valid|invalid|empty raw profile' "$grcov_err"; then
      echo "cov: grcov could not read some profiles and carried on anyway:" >&2
      cat "$grcov_err" >&2
      echo "     Every skipped profile is coverage missing from the report, so the" >&2
      echo "     number below would be wrong rather than low." >&2
      exit 1
    fi

    case "$(uname -s)" in
      Darwin) label_os=darwin ;;
      *)      label_os=linux ;;
    esac
    case "$(uname -m)" in
      arm64|aarch64) label_arch=arm64 ;;
      *)             label_arch=amd64 ;;
    esac
    label="$label_os/$label_arch"

    echo
    echo "> cov  $label - $tests_run tests, $profraw_count profraw ($(du -sh "$profraw" 2>/dev/null | cut -f1))"
    echo "       doctests run but are not instrumented on stable, so they count for nothing here"
    df -h "$root" | tail -n 1

    # `--strip-cfg-test` is what makes the number mean anything here: 39% of this
    # tree's Rust lines sit inside `#[cfg(test)]` modules, and source-based
    # coverage instruments them along with the code they test. Path-based
    # exclusion cannot reach them — they are inside production files — so the
    # filtering happens here, against the source, and the filtered report is what
    # Codecov reads. See the script's header for why this is not grcov's
    # `--excl-start`.
    #
    # The floors and the canary are what stop an empty report from being
    # published as a number. Nothing downstream can: `codecov.yml` is
    # `informational`, and the job is deliberately not in `release`'s `needs:` —
    # so if this script does not go red, nothing does.
    if ! summary="$(python3 "$root/scripts/coverage-report.py" \
      "$raw" \
      --source-root "$root" \
      --strip-cfg-test \
      --out-lcov "$out/lcov.info" \
      --json "$out/summary.json" \
      --label "$label" \
      ''${floors[@]+"''${floors[@]}"})"
    then
      # stdout was captured, so the table has to be replayed here or the failure
      # message arrives with nothing to read it against. The profiles are left
      # in place for the same reason.
      printf '%s\n' "$summary" >&2
      exit 1
    fi
    printf '%s\n' "$summary"

    # The profiles are the largest thing this run produced and grcov has already
    # read them. On the CI runner that `Free disk space` step exists for, they
    # are the difference between the next step running and ENOSPC.
    rm -rf "$profraw"

    echo
    echo "  lcov     $out/lcov.info      (filtered; what Codecov reads)"
    echo "  raw      $out/lcov.raw.info  (grcov's output, #[cfg(test)] included)"
    echo "  summary  $out/summary.json"
    echo "  log      $out/test.log"

    # What someone reads first when the number moves or the job goes red —
    # before Codecov, and without leaving the run.
    if [ -n "''${GITHUB_STEP_SUMMARY:-}" ]; then
      {
        echo "## Coverage $label"
        echo
        echo "$tests_run tests, $profraw_count profraw. \`#[cfg(test)]\` modules and doctests excluded."
        echo
        echo '```'
        printf '%s\n' "$summary"
        echo '```'
      } >> "$GITHUB_STEP_SUMMARY"
    fi

    # Last, so the report is produced and uploaded either way. A red suite is
    # reported by `test` too; what this adds is the coverage of the run that
    # failed, which is often what tells you why.
    if [ "$suite_rc" -ne 0 ]; then
      echo "cov: exiting $suite_rc because the test suite failed (see above)." >&2
      exit "$suite_rc"
    fi
  '';

  # Binary end-to-end suite: black-box tests against the artifacts CI publishes
  # (the `heph` binary + the go/gha plugin cdylibs). ONE entrypoint, identical
  # locally and in CI — the only difference is where the artifacts come from:
  #
  #   e2e                      # build them from this tree (local default)
  #   HEPH_E2E_FROM=dist e2e   # use an already-downloaded set (CI)
  #
  # Both branches converge on the same normalized layout, so the tests never
  # learn which one ran. Extra args pass through to cargo test (e.g.
  # `e2e tui_pty -- --nocapture`).
  scripts.e2e.exec = ''
    set -euo pipefail

    case "$(uname -s)" in
      Darwin) os=darwin; ext=dylib ;;
      *)      os=linux;  ext=so ;;
    esac
    case "$(uname -m)" in
      arm64|aarch64) arch=arm64 ;;
      *)             arch=amd64 ;;
    esac

    target="$(target-dir)"

    # Stage into a directory unique to THIS run. Worktrees no longer share one
    # target dir, but two `e2e` runs in the *same* worktree — another terminal,
    # or a re-run started before the first finished — still collide: a fixed
    # path would let the second `rm -rf` the binaries the first is still running
    # tests against, and the failure would surface as an unrelated test blowing
    # up somewhere else. mktemp costs one copy of three files and removes the
    # whole class.
    dist_root="$target/e2e-dist"
    mkdir -p "$dist_root"
    dist="$(mktemp -d "$dist_root/run.XXXXXXXX")"
    # Keep the staged artifacts for inspection with HEPH_E2E_KEEP_DIST=1.
    if [ -z "''${HEPH_E2E_KEEP_DIST:-}" ]; then
      trap 'rm -rf "$dist"' EXIT
    else
      trap 'echo "staged artifacts kept at $dist"' EXIT
    fi

    if [ -n "''${HEPH_E2E_FROM:-}" ]; then
      # CI: artifacts downloaded from the `build` job, still carrying their
      # per-platform names. Strip the suffix so the tests see one layout.
      src="$HEPH_E2E_FROM"
      cp "$src/heph_''${os}_''${arch}"                 "$dist/heph"
      cp "$src/heph-go-plugin_''${os}_''${arch}.$ext"  "$dist/heph-go-plugin.$ext"
      cp "$src/heph-gha-plugin_''${os}_''${arch}.$ext" "$dist/heph-gha-plugin.$ext"
      cp "$src/heph-oci-plugin_''${os}_''${arch}.$ext" "$dist/heph-oci-plugin.$ext"
      cp "$src/heph-devenv-plugin_''${os}_''${arch}.$ext" "$dist/heph-devenv-plugin.$ext"
    else
      # Local: build the same artifacts the build job builds, the same way (one
      # invocation so cargo overlaps their LTO tails — see heph.yml).
      cargo build --release --locked --bin heph --lib -p heph -p plugin-go-cdylib -p plugin-gha-cdylib -p plugin-oci-cdylib -p plugin-devenv-cdylib
      out="$target/release"

      # cargo's build lock covers the build but not the gap between it and the
      # copy below. Worktrees no longer share `release/`, which removes the
      # cross-worktree case — but a second build in *this* worktree (another
      # terminal, an editor's check-on-save, a rust-analyzer run) landing in
      # that gap still hands this run different bytes, and every assertion would
      # still pass. Fingerprint the artifacts around the copy so that becomes a
      # loud failure instead of a green run against the wrong binary.
      # Selected by which `stat` is on PATH, not by `$os`: the devenv shell puts
      # GNU coreutils ahead of /usr/bin on macOS too, so keying this off `darwin`
      # ran BSD syntax against GNU stat (`-f` there means "filesystem") and every
      # local `e2e` on a Mac died before running a single test.
      fingerprint() {
        stat -c '%i %s %Y' "$@" 2>/dev/null || stat -f '%i %z %m' "$@"
      }
      before="$(fingerprint "$out/heph" "$out/libplugin_go_cdylib.$ext" "$out/libplugin_gha_cdylib.$ext" "$out/libplugin_oci_cdylib.$ext" "$out/libplugin_devenv_cdylib.$ext")"

      cp "$out/heph"                       "$dist/heph"
      cp "$out/libplugin_go_cdylib.$ext"   "$dist/heph-go-plugin.$ext"
      cp "$out/libplugin_gha_cdylib.$ext"  "$dist/heph-gha-plugin.$ext"
      cp "$out/libplugin_oci_cdylib.$ext"  "$dist/heph-oci-plugin.$ext"
      cp "$out/libplugin_devenv_cdylib.$ext" "$dist/heph-devenv-plugin.$ext"

      after="$(fingerprint "$out/heph" "$out/libplugin_go_cdylib.$ext" "$out/libplugin_gha_cdylib.$ext" "$out/libplugin_oci_cdylib.$ext" "$out/libplugin_devenv_cdylib.$ext")"
      if [ "$before" != "$after" ]; then
        echo "e2e: $out changed while staging — another build in this" >&2
        echo "e2e: worktree raced this one. Re-run." >&2
        exit 1
      fi

      if [ "$os" = "darwin" ]; then
        # Same post-processing the shipped macOS artifacts get, so a local run
        # tests the same bytes CI would publish.
        for f in "$dist/heph" "$dist/heph-go-plugin.$ext" "$dist/heph-gha-plugin.$ext" "$dist/heph-oci-plugin.$ext" "$dist/heph-devenv-plugin.$ext"; do
          bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$f"
        done
      fi

      # Stamp the version slot, for the same reason as the macOS step above:
      # the shipped artifacts get this patch (see the `build` job in
      # .github/workflows/heph.yml), so a local run must too. The version is no
      # longer compiled in, so without this a locally staged binary reports
      # `v0.0.0-dev` — and `crates/bin-e2e`'s assertion that a shipped artifact
      # is never the dev sentinel would fail locally while passing in CI, which
      # is the exact CI-vs-local split that assertion exists to prevent.
      #
      # After macos-portable.sh, not before: both re-sign, and this one must
      # have the last word or the signature covers pre-patch bytes.
      version="$(bash "$DEVENV_ROOT/.github/workflows/version.sh")"
      for f in "$dist/heph" "$dist/heph-go-plugin.$ext" "$dist/heph-gha-plugin.$ext" "$dist/heph-oci-plugin.$ext" "$dist/heph-devenv-plugin.$ext"; do
        bash "$DEVENV_ROOT/scripts/patch-slot.sh" "$f" "version=$version"
      done
    fi

    # download-artifact does not preserve the executable bit.
    chmod +x "$dist/heph"

    export HEPH_E2E_DIST="$dist"
    # --no-fail-fast: each test file is a separate binary, and cargo stops at the
    # first one that fails. A CI run that spends 20 minutes building artifacts
    # should report every broken seam it found, not just the first.
    cargo test --locked -p bin-e2e --no-fail-fast "''${@}"
  '';

  # Cargo's target directory for *this checkout* — the one cargo writes to,
  # asked rather than assumed.
  #
  # Anchored at `$DEVENV_ROOT`, not `$PWD`. Every caller means "the heph I
  # built", and heph is a build tool you deliberately run against some other
  # project: `cd ~/someproject && run-release build //...`. Resolving from
  # `$PWD` breaks that outright outside a cargo workspace, and does something
  # worse inside one — it silently resolves to *that* project's `target/` and
  # looks for heph there.
  #
  # `locate-project` rather than a bare `$DEVENV_ROOT/target` so the answer
  # still comes from cargo; it resolves no dependencies, so this is cheap.
  scripts.target-dir.exec = ''
    set -euo pipefail
    root="$(cd "$DEVENV_ROOT" && cargo locate-project --workspace --message-format plain)"
    echo "''${root%/Cargo.toml}/target"
  '';

  scripts.build-profile.exec = ''cargo build --profile profiling'';
  scripts.run-profile.exec = ''"$(target-dir)"/profiling/heph "''${@}"'';
  scripts.run-samply-profile.exec = ''samply record --unstable-presymbolicate "$(target-dir)"/profiling/heph "''${@}"'';

  scripts.build-release.exec = ''cargo build --profile release'';
  scripts.run-release.exec = ''"$(target-dir)"/release/heph "''${@}"'';

  scripts.rheph.exec = ''cargo run -q --profile release -- "''${@}"'';
  scripts.pheph.exec = ''cargo run -q --profile profiling -- "''${@}"'';
  scripts.dheph.exec = ''cargo run -q --profile dev -- "''${@}"'';

  # Start a Claude Code session in HEPH release-candidate mode: HEPH_RC=1
  # triggers the SessionStart hook (checkout master + ff-only pull) and the
  # session opens in a fresh git worktree.
  scripts.ccrc.exec = ''HEPH_RC=1 claude rc --spawn=worktree "''${@}"'';

  scripts.rsync-to.exec = ''cd $DEVENV_ROOT && rsync -avz --exclude='.heph3/' --exclude='.claude/' --exclude='**/.claude/' --exclude='target/' --exclude='.devenv/' --exclude='.git/' $DEVENV_ROOT/ "''${@}"'';

  scripts.install-dev.exec = ''
    sed "s|<HEPH_SRC_ROOT>|$(pwd)|g" < $DEVENV_ROOT/scripts/dev.sh > /tmp/heph
    chmod +x /tmp/heph
    mkdir -p $(dirname "${binLocation}")
    mv /tmp/heph "${binLocation}"
  '';

  # Install the go plugin (cdylib + manifest) into the user-global ~/.heph dir, so
  # an installed `heph3` can load it from any workspace via
  # `plugins: - { identifier: { path: ~/.heph/plugins/go/heph-go-plugin.json } }`.
  # Always a release build — it's a runtime artifact. The cdylib keeps its native
  # extension (.so on Linux, .dylib on macOS); the manifest (one host artifact,
  # path = the sibling cdylib) is emitted by tools/pluginmanifest.
  scripts.install-go-plugin.exec = ''
    cargo build --release -p plugin-go-cdylib
    target="$(target-dir)"
    if [ "$(uname -s)" = "Darwin" ]; then
      lib="$target/release/libplugin_go_cdylib.dylib"
      name="heph-go-plugin.dylib"
      bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$lib"
    else
      lib="$target/release/libplugin_go_cdylib.so"
      name="heph-go-plugin.so"
    fi
    dest="$HOME/.heph/plugins/go"
    mkdir -p "$dest"
    cp "$lib" "$dest/$name.new"
    mv -f "$dest/$name.new" "$dest/$name"
    # `-host-path` is the sibling basename recorded in the manifest (heph
    # resolves it against the manifest dir); `-checksum-from` is the real file
    # to hash — the just-installed dylib at $dest, which is NOT under this tool's
    # cwd, so the two must be passed separately.
    ( cd "$DEVENV_ROOT/tools/pluginmanifest" \
        && go run . -name go -host-path "$name" -checksum-from "$dest/$name" -out "$dest/heph-go-plugin.json" )
    echo "installed go plugin -> $dest"
  '';

  # Build + install the GitHub Actions hook plugin (a cdylib + manifest), the same
  # publish flow as `install-go-plugin`. Reference it from config with a `path:`
  # entry (e.g. in a `ci.hephconfig` profile overlay, enabled via HEPH_PROFILES).
  scripts.install-gha-plugin.exec = ''
    cargo build --release -p plugin-gha-cdylib
    target="$(target-dir)"
    if [ "$(uname -s)" = "Darwin" ]; then
      lib="$target/release/libplugin_gha_cdylib.dylib"
      name="heph-gha-plugin.dylib"
      bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$lib"
    else
      lib="$target/release/libplugin_gha_cdylib.so"
      name="heph-gha-plugin.so"
    fi
    dest="$HOME/.heph/plugins/gha"
    mkdir -p "$dest"
    cp "$lib" "$dest/$name.new"
    mv -f "$dest/$name.new" "$dest/$name"
    ( cd "$DEVENV_ROOT/tools/pluginmanifest" \
        && go run . -name gha -host-path "$name" -checksum-from "$dest/$name" -out "$dest/heph-gha-plugin.json" )
    echo "installed gha plugin -> $dest"
  '';

  # Build + install the devenv exec-runner plugin (a cdylib + manifest), the same
  # publish flow as `install-gha-plugin`. Reference it from config with a
  # `path: ~/.heph/plugins/devenv/heph-devenv-plugin.json` entry.
  # Build + install the OCI plugin (a cdylib + manifest), the same publish flow
  # as `install-gha-plugin`. Reference it from config with a
  # `path: ~/.heph/plugins/oci/heph-oci-plugin.json` entry.
  scripts.install-oci-plugin.exec = ''
    cargo build --release -p plugin-oci-cdylib
    target="$(target-dir)"
    if [ "$(uname -s)" = "Darwin" ]; then
      lib="$target/release/libplugin_oci_cdylib.dylib"
      name="heph-oci-plugin.dylib"
      bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$lib"
    else
      lib="$target/release/libplugin_oci_cdylib.so"
      name="heph-oci-plugin.so"
    fi
    dest="$HOME/.heph/plugins/oci"
    mkdir -p "$dest"
    cp "$lib" "$dest/$name.new"
    mv -f "$dest/$name.new" "$dest/$name"
    ( cd "$DEVENV_ROOT/tools/pluginmanifest" \
        && go run . -name oci -host-path "$name" -checksum-from "$dest/$name" -out "$dest/heph-oci-plugin.json" )
    echo "installed oci plugin -> $dest"
  '';

  scripts.install-devenv-plugin.exec = ''
    cargo build --release -p plugin-devenv-cdylib
    target="$(target-dir)"
    if [ "$(uname -s)" = "Darwin" ]; then
      lib="$target/release/libplugin_devenv_cdylib.dylib"
      name="heph-devenv-plugin.dylib"
      bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$lib"
    else
      lib="$target/release/libplugin_devenv_cdylib.so"
      name="heph-devenv-plugin.so"
    fi
    dest="$HOME/.heph/plugins/devenv"
    mkdir -p "$dest"
    cp "$lib" "$dest/$name.new"
    mv -f "$dest/$name.new" "$dest/$name"
    ( cd "$DEVENV_ROOT/tools/pluginmanifest" \
        && go run . -name devenv -host-path "$name" -checksum-from "$dest/$name" -out "$dest/heph-devenv-plugin.json" )
    echo "installed devenv plugin -> $dest"
  '';

  # The counterpart to every `install-<name>-plugin` above: instead of building a
  # cdylib from this tree, install the plugin manifests published alongside the
  # *installed* binary — the release `heph version` reports.
  #
  # It matters which one you want. A cdylib and the host are linked at load time
  # through stabby, so a plugin built from a different commit than the binary
  # loading it fails the ABI check with a page of type-report text and no hint
  # about the cause. Building from source is right while developing a plugin;
  # this is right for running an installed `heph` (the `example/` workspace, say,
  # whose `.hephconfig2` names the go, devenv and oci plugins by path).
  #
  # Which plugins it installs is whatever the release published, not a list kept
  # here — a new plugin needs no change. The released manifests reference their
  # cdylibs by `url` + `sha256`, so nothing is downloaded per-platform here:
  # heph pulls and verifies the one matching the host on first load.
  #
  #   install-release-plugins                 # from `heph version`
  #   install-release-plugins --bin heph3     # ask a different binary
  #   install-release-plugins --version vX    # skip asking, pin a tag
  #   install-release-plugins --dry-run
  scripts.install-release-plugins.exec = ''
    python3 "$DEVENV_ROOT/scripts/install-release-plugins.py" "''${@}"
  '';

  scripts.install-dev-build.exec = ''
    cargo build
    mkdir -p $(dirname "${binLocation}")
    # Atomic replace (new inode) — overwriting the binary in place leaves macOS
    # holding the previous code-signature for that path and SIGKILLs the next run.
    cp "$(target-dir)"/debug/heph "${binLocation}.new"
    mv -f "${binLocation}.new" "${binLocation}"
    install-go-plugin
  '';

  scripts.install-release-build.exec = ''
    cargo build --release
    bin="$(target-dir)/release/heph"
    if [ "$(uname -s)" = "Darwin" ]; then
      # The nix toolchain hard-links libiconv against its /nix/store path, which
      # dyld aborts on once that store path is GC'd ("Killed"). Rewrite to the
      # OS /usr/lib copy and re-sign ad-hoc so the installed binary keeps
      # launching — same treatment the shipped CI artifact gets.
      bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$bin"
    fi
    mkdir -p $(dirname "${binLocation}")
    # Atomic replace (new inode): overwriting in place keeps macOS's cached
    # code-signature for the old bytes, which SIGKILLs the next run on Apple
    # Silicon. `mv` swaps the path to a fresh inode so AMFI re-validates.
    cp "$bin" "${binLocation}.new"
    mv -f "${binLocation}.new" "${binLocation}"
    install-go-plugin
  '';


  # https://devenv.sh/basics/
  #
  # No `CARGO_TARGET_DIR` export. Every worktree used to be pointed at one
  # shared target dir so dependencies were compiled once rather than per
  # worktree — a job kache now does properly: it keys on content rather than on
  # a directory, shares across worktrees *and* machines through R2, and on a
  # copy-on-write filesystem (APFS, btrfs, XFS-with-reflink) a restored
  # `target/` costs almost no additional disk. Cargo's own per-workspace
  # default is what runs now.
  #
  # The override was not free. One directory written by concurrent builds from
  # different worktrees is a race: `scripts.e2e` still fingerprints `release/`
  # around its copy because a build from elsewhere landing in that window would
  # otherwise hand it another branch's binary with every assertion still
  # passing. It also left `target-verify/` behind as a workaround for the same
  # poisoning. Scripts locate the target dir with `target-dir` instead, which
  # asks cargo rather than assuming.
  enterShell = "";

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
