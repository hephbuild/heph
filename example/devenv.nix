# The environment the `//exec_runner:*` examples build in.
#
# It lives at the workspace root because that is where `devenv` looks: the
# `devenv` driver runs `devenv print-dev-env` against the tree root, so a
# workspace has one devenv environment, not one per package.
{ pkgs, ... }:
{
  # On PATH inside the environment, and NOT on the host's — which is what the
  # `needs_jq` / `no_jq_without_runner` pair demonstrates.
  packages = [ pkgs.jq ];

  # Reported by `devenv print-dev-env`, so it is captured into the snapshot and
  # is part of every consuming target's cache key.
  env.FROM_DEVENV_NIX = "captured";
}
