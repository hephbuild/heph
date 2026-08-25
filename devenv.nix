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
  qualityCrates = "-p heph -p e2e -p bin-e2e -p testkit -p plugingo-e2e -p htspec-derive -p core -p config -p walk -p proc -p execrunner -p model -p sandboxfuse -p plugin -p plugin-abi -p plugin-sdk -p plugin-stabby -p plugin-go-cdylib -p builtins -p plugin-buildfile -p driver-support -p driver-bridge -p plugin-exec -p plugin-nix -p plugin-http -p plugin-oci -p plugin-query -p plugin-go -p plugin-gha -p plugin-gha-cdylib -p plugin-oci-cdylib -p telemetry -p tui -p lock -p selfupdate -p engine -p xstarlark-fmt -p bench-corpus -p bench";
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
    else
      # Local: build the same artifacts the build job builds, the same way (one
      # invocation so cargo overlaps their LTO tails — see heph.yml).
      cargo build --release --locked --bin heph --lib -p heph -p plugin-go-cdylib -p plugin-gha-cdylib -p plugin-oci-cdylib
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
      before="$(fingerprint "$out/heph" "$out/libplugin_go_cdylib.$ext" "$out/libplugin_gha_cdylib.$ext" "$out/libplugin_oci_cdylib.$ext")"

      cp "$out/heph"                       "$dist/heph"
      cp "$out/libplugin_go_cdylib.$ext"   "$dist/heph-go-plugin.$ext"
      cp "$out/libplugin_gha_cdylib.$ext"  "$dist/heph-gha-plugin.$ext"
      cp "$out/libplugin_oci_cdylib.$ext"  "$dist/heph-oci-plugin.$ext"

      after="$(fingerprint "$out/heph" "$out/libplugin_go_cdylib.$ext" "$out/libplugin_gha_cdylib.$ext" "$out/libplugin_oci_cdylib.$ext")"
      if [ "$before" != "$after" ]; then
        echo "e2e: $out changed while staging — another build in this" >&2
        echo "e2e: worktree raced this one. Re-run." >&2
        exit 1
      fi

      if [ "$os" = "darwin" ]; then
        # Same post-processing the shipped macOS artifacts get, so a local run
        # tests the same bytes CI would publish.
        for f in "$dist/heph" "$dist/heph-go-plugin.$ext" "$dist/heph-gha-plugin.$ext" "$dist/heph-oci-plugin.$ext"; do
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
      for f in "$dist/heph" "$dist/heph-go-plugin.$ext" "$dist/heph-gha-plugin.$ext" "$dist/heph-oci-plugin.$ext"; do
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
