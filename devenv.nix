{ pkgs, lib, config, inputs, ... }:

let
  binLocation = "$HOME/.local/bin/heph3";
  qualityCrates = "-p heph -p e2e -p heph-testkit -p plugingo-e2e -p htspec-derive";
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
  scripts.lint.exec = "echo '> clippy' && cargo clippy --all-targets --locked -- -D warnings && echo '> fmt' && cargo fmt --check ${qualityCrates}";
  scripts.fix.exec = "cargo fix --allow-dirty && cargo fmt ${qualityCrates}";
  scripts.tst.exec = "cargo test --locked --all";

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

  scripts.install-dev-build.exec = ''
    cargo build
    mkdir -p $(dirname "${binLocation}")
    cp $CARGO_TARGET_DIR/debug/heph "${binLocation}"
  '';

  scripts.install-release-build.exec = ''
    cargo build --release
    mkdir -p $(dirname "${binLocation}")
    cp $CARGO_TARGET_DIR/release/heph "${binLocation}"
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
