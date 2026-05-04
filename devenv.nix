{ pkgs, lib, config, inputs, ... }:

let
  # 1. Define the location in one place here
  # This is a literal string that will be injected into your scripts
  binLocation = "$HOME/.local/bin/rheph";
in
{
  # https://devenv.sh/basics/
  env.BIN_LOCATION = "$HOME/.local/bin/rheph";

  # https://devenv.sh/packages/
  packages = [
    pkgs.git
    pkgs.buf
    pkgs.protoc-gen-prost
    pkgs.protoc-gen-prost-serde
    pkgs.protoc-gen-prost-crate
    pkgs.zig
    pkgs.cargo-zigbuild
  ];

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
  scripts.proto-gen.exec = "buf generate";
  scripts.gen.exec = "proto-gen";
  scripts.lint.exec = "cargo clippy -- -D warnings";
  scripts.test.exec = "cargo test --all";

  scripts.install-dev.exec = ''
    sed "s|<HEPH_SRC_ROOT>|$(pwd)|g" < $DEVENV_ROOT/scripts/dev.sh > /tmp/heph
    chmod +x /tmp/heph
    mkdir -p $(dirname "${binLocation}")
    mv /tmp/heph "${binLocation}"
  '';

  scripts.install-dev-build.exec = ''
    cargo build --target-dir /tmp/rheph
    mkdir -p $(dirname "${binLocation}")
    mv /tmp/rheph/debug/rheph "${binLocation}"
  '';

  scripts.install-release-build.exec = ''
    cargo build --release --target-dir /tmp/rheph
    mkdir -p $(dirname "${binLocation}")
    mv /tmp/rheph/release/rheph "${binLocation}"
  '';


  # https://devenv.sh/basics/
#  enterShell = ''
#    hello         # Run scripts directly
#    git --version # Use packages
#  '';

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
