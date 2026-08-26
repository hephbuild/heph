# The build toolchain, pinned.
#
# Nothing here is installed on a developer's machine — `jq` and `yq-go` exist
# only inside this environment. That is the point of the example: the targets
# in `app/BUILD` use them, and they work on a laptop that has never heard of
# either.
{ pkgs, ... }: {
  packages = [ pkgs.jq pkgs.yq-go ];

  # Reaches every target that runs under this runner, and — because the runner
  # target's output is hashed — changing it re-keys all of them.
  env.APP_CHANNEL = "stable";
}
