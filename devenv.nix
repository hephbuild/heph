{ pkgs, lib, config, inputs, ... }:

let
  binLocation = "$HOME/.local/bin/heph3";
  qualityCrates = "-p heph -p e2e -p testkit -p plugingo-e2e -p htspec-derive -p core -p config -p walk -p proc -p model -p sandboxfuse -p plugin -p plugin-abi -p plugin-sdk -p plugin-stabby -p plugin-go-cdylib -p builtins -p plugin-buildfile -p driver-support -p driver-bridge -p plugin-exec -p plugin-nix -p plugin-query -p plugin-go -p telemetry -p tui -p lock -p selfupdate -p engine";
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
  scripts.tst.exec = "cargo test --locked --all && cargo test --locked -p plugin-stabby --features host && cargo test --locked -p plugin-sdk --features stabby";

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
