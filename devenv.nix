{ pkgs, lib, config, inputs, ... }:

let
  binLocation = "$HOME/.local/bin/heph3";
  qualityCrates = "-p heph -p e2e -p bin-e2e -p testkit -p plugingo-e2e -p htspec-derive -p core -p config -p walk -p proc -p model -p sandboxfuse -p plugin -p plugin-abi -p plugin-sdk -p plugin-stabby -p plugin-go-cdylib -p builtins -p plugin-buildfile -p driver-support -p driver-bridge -p plugin-exec -p plugin-nix -p plugin-http -p plugin-query -p plugin-go -p plugin-gha -p plugin-gha-cdylib -p telemetry -p tui -p lock -p selfupdate -p engine -p xstarlark-fmt";
in
{
  # https://devenv.sh/basics/

  # https://devenv.sh/packages/
  packages = [
    pkgs.git
    pkgs.buf
    pkgs.protoc-gen-prost
    pkgs.protoc-gen-prost-serde
    pkgs.protoc-gen-prost-crate
    pkgs.zig
    pkgs.cargo-zigbuild
    pkgs.tokio-console
    pkgs.sccache
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

  # Route every rustc invocation through sccache (local + CI, since CI runs
  # inside this shell). SCCACHE_DIR is left at its platform default locally;
  # CI overrides it to a workspace path so it can be cached across runs.
  env.RUSTC_WRAPPER = "sccache";

  # https://devenv.sh/languages/
   languages.rust = {
     enable = true;
     channel = "stable";
     components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-analyzer" ];
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
  # Lint default-feature code, then again with every feature enabled (so
  # feature-gated code — the stabby host loader — is covered too), then fmt-check
  # all hand-written crates (qualityCrates; generated gen/proto is excluded).
  scripts.lint.exec = "echo '> clippy' && cargo clippy --all-targets --locked -- -D warnings && echo '> clippy --all-features' && cargo clippy --all-targets --all-features --locked -- -D warnings && echo '> fmt' && cargo fmt --check ${qualityCrates}";
  scripts.fix.exec = "cargo fix --allow-dirty && cargo fmt ${qualityCrates}";
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

    # Stage into a directory unique to THIS run. CARGO_TARGET_DIR is inherited
    # from the environment (see enterShell) and worktrees routinely share one,
    # so a fixed path under it is not private to this run: a second `e2e` — in
    # another worktree, or just another terminal — would `rm -rf` the binaries
    # the first one is still running tests against, and the failure would
    # surface as an unrelated test blowing up somewhere else. mktemp costs one
    # copy of three files and removes the whole class.
    dist_root="$CARGO_TARGET_DIR/e2e-dist"
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
    else
      # Local: build the same three artifacts the build job builds, the same way
      # (one invocation so cargo overlaps the three LTO tails — see heph.yml).
      cargo build --release --locked --bin heph --lib -p heph -p plugin-go-cdylib -p plugin-gha-cdylib
      out="$CARGO_TARGET_DIR/release"

      # `release/` is shared across worktrees too, and cargo's build lock only
      # covers the build — not the gap between it and the copy below. A build in
      # another worktree landing in that gap would hand this run some other
      # branch's binary, and every assertion would still pass. Fingerprint the
      # artifacts around the copy so that becomes a loud failure instead of a
      # green run against the wrong bytes.
      fingerprint() {
        if [ "$os" = "darwin" ]; then stat -f '%i %z %m' "$@"; else stat -c '%i %s %Y' "$@"; fi
      }
      before="$(fingerprint "$out/heph" "$out/libplugin_go_cdylib.$ext" "$out/libplugin_gha_cdylib.$ext")"

      cp "$out/heph"                       "$dist/heph"
      cp "$out/libplugin_go_cdylib.$ext"   "$dist/heph-go-plugin.$ext"
      cp "$out/libplugin_gha_cdylib.$ext"  "$dist/heph-gha-plugin.$ext"

      after="$(fingerprint "$out/heph" "$out/libplugin_go_cdylib.$ext" "$out/libplugin_gha_cdylib.$ext")"
      if [ "$before" != "$after" ]; then
        echo "e2e: $out changed while staging — another build (likely another" >&2
        echo "e2e: worktree sharing CARGO_TARGET_DIR) raced this one. Re-run." >&2
        exit 1
      fi

      if [ "$os" = "darwin" ]; then
        # Same post-processing the shipped macOS artifacts get, so a local run
        # tests the same bytes CI would publish.
        for f in "$dist/heph" "$dist/heph-go-plugin.$ext" "$dist/heph-gha-plugin.$ext"; do
          bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$f"
        done
      fi
    fi

    # download-artifact does not preserve the executable bit.
    chmod +x "$dist/heph"

    export HEPH_E2E_DIST="$dist"
    # --no-fail-fast: each test file is a separate binary, and cargo stops at the
    # first one that fails. A CI run that spends 20 minutes building artifacts
    # should report every broken seam it found, not just the first.
    cargo test --locked -p bin-e2e --no-fail-fast "''${@}"
  '';

  scripts.build-profile.exec = ''cargo build --profile profiling'';
  scripts.run-profile.exec = ''$CARGO_TARGET_DIR/profiling/heph "''${@}"'';
  scripts.run-samply-profile.exec = ''samply record --unstable-presymbolicate $CARGO_TARGET_DIR/profiling/heph "''${@}"'';

  scripts.build-release.exec = ''cargo build --profile release'';
  scripts.run-release.exec = ''$CARGO_TARGET_DIR/release/heph "''${@}"'';

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
    if [ "$(uname -s)" = "Darwin" ]; then
      lib="$CARGO_TARGET_DIR/release/libplugin_go_cdylib.dylib"
      name="heph-go-plugin.dylib"
      bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$lib"
    else
      lib="$CARGO_TARGET_DIR/release/libplugin_go_cdylib.so"
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
    if [ "$(uname -s)" = "Darwin" ]; then
      lib="$CARGO_TARGET_DIR/release/libplugin_gha_cdylib.dylib"
      name="heph-gha-plugin.dylib"
      bash "$DEVENV_ROOT/scripts/macos-portable.sh" "$lib"
    else
      lib="$CARGO_TARGET_DIR/release/libplugin_gha_cdylib.so"
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
    cp $CARGO_TARGET_DIR/debug/heph "${binLocation}.new"
    mv -f "${binLocation}.new" "${binLocation}"
    install-go-plugin
  '';

  scripts.install-release-build.exec = ''
    cargo build --release
    bin="$CARGO_TARGET_DIR/release/heph"
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
  enterShell = ''
    # All git worktrees share one cargo target dir (deps stored once, not
    # duplicated per worktree). The shell is rooted at the MAIN checkout, so
    # $DEVENV_ROOT is stable across every worktree a tool call cd's into; the
    # exported var is inherited by all subprocesses. Respect an externally-set
    # value (CI pins ./target).
    export CARGO_TARGET_DIR="''${CARGO_TARGET_DIR:-$DEVENV_ROOT/target}"
  '';

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
