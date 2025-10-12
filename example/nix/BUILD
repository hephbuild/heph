target(
    name = "cowsay",
    driver = "nix-bash",
    tools = "//@nix:cowsay",
    run = "cowsay hello",
    cache = False,
)

target(
    name = "go-version",
    driver = "nix-bash",
    tools = "//@nix:go",
    run = "go version",
    cache = False,
)
