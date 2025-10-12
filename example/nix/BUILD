target(
    name = "cowsay-nix",
    driver = "nix-bash",
    tools = "//@nix:cowsay",
    run = "cowsay hello",
    cache = False,
)

target(
    name = "go-version-nix",
    driver = "nix-bash",
    tools = "//@nix:go",
    run = "go version",
    cache = False,
)

target(
    name = "cowsay",
    driver = "bash",
    tools = "//@nix:cowsay",
    run = "cowsay hello",
    cache = False,
)

target(
    name = "go-version",
    driver = "bash",
    tools = "//@nix:go",
    run = "go version",
    cache = False,
)
