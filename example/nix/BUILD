# nix driver smoke target.
#
# Requires `nix` on PATH (any version with `nix build` / flakes).
# For fast repeat evals, add to ~/.config/nix/nix.conf:
#   experimental-features = nix-command flakes lazy-trees
#   lazy-trees = true
#
# `nixpkgs` should pin a commit. A moving branch keeps returning a wrapper
# pointing at the originally-resolved store path until the URL changes.

nix_tools = target(
    name = "nix_tools",
    driver = "nix",
    nixpkgs = "github:NixOS/nixpkgs/nixos-unstable",
    packages = ["pkgs.ripgrep"],
    programs = ["rg"],
)

target(
    name = "check",
    driver = "bash",
    # Multi-output tool targets (one Output per program) are referenced
    # directly — pluginexec symlinks each program file into the bin dir on
    # PATH. Pick a single program with `|<name>` if needed.
    tools = nix_tools,
    run = ["rg --version > $OUT"],
    out = "check.txt",
    cache = False,
)
