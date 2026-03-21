{ pkgs, lib, config, inputs, ... }:

{
  # https://devenv.sh/basics/
  env.BIN_LOCATION = "~/.local/bin/rheph";

  # https://devenv.sh/packages/
  packages = [
    pkgs.git
    pkgs.buf
    pkgs.protoc-gen-prost
    pkgs.protoc-gen-prost-serde
    pkgs.protoc-gen-prost-crate
  ];

  # https://devenv.sh/languages/
   languages.rust = {
     enable = true;
     components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-analyzer" ];
   };

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/scripts/
  scripts.proto-gen.exec = "buf generate";
  scripts.gen.exec = "proto-gen";

  scripts.install-dev.exec = ''
    location=$BIN_LOCATION
    sed "s|<HEPH_SRC_ROOT>|$(pwd)|g" < scripts/dev.sh > /tmp/heph
    chmod +x /tmp/heph
    mkdir -p $(dirname "$location")
    mv /tmp/heph "$location"
  '';

  scripts.install-dev-build.exec = ''
    location=$BIN_LOCATION
    cargo build --target-dir /tmp/rheph
    mkdir -p $(dirname "$location")
    mv /tmp/rheph/debug/rheph "$location"
  '';


  # https://devenv.sh/basics/
#  enterShell = ''
#    hello         # Run scripts directly
#    git --version # Use packages
#  '';

  # https://devenv.sh/tasks/
  tasks = {
    "heph:generate".exec = "gen";
    "devenv:enterShell".after = [ "heph:generate" ];
  };

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
