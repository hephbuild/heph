# nix-bash
target(
    name = "cowsay-nix-bash",
    driver = "nix-bash",
    tools = "//@nix:cowsay",
    run = "cowsay hello",
    cache = False,
)

target(
    name = "go-version-nix-bash",
    driver = "nix-bash",
    tools = "//@nix:go",
    run = "go version",
    cache = False,
)

# auto nix tool, normal bash
target(
    name = "cowsay-auto",
    driver = "bash",
    tools = "//@nix:cowsay",
    run = "cowsay hello",
    cache = False,
)

target(
    name = "go-version-auto",
    driver = "bash",
    tools = "//@nix:go",
    run = "go version",
    cache = False,
)

# nix-wrapper
target(
    name = "cowsay-tool-wrapper",
    driver = "nix-wrapper",
    packages = ["cowsay"],
)

target(
    name = "cowsay-nix",
    driver = "bash",
    tools = "//nix:cowsay-tool-wrapper",
    run = "cowsay hello",
    cache = False,
)

target(
    name = "go-version-tool-wrapper",
    driver = "nix-wrapper",
    packages = ["go"],
)

target(
    name = "go-version-wrapper",
    driver = "bash",
    tools = "//nix:go-version-tool-wrapper",
    run = "go version",
    cache = False,
)

