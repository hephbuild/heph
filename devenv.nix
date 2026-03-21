{ pkgs, lib, config, inputs, ... }:

{
  # https://devenv.sh/basics/
  env.GREET = "devenv";

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
