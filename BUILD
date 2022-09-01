target(
    name="e2e_test",
    run="heph --log_level=trace query --include e2e_test | heph run -",
    tools=['heph'],
    pass_env=["PATH"],
    sandbox=False,
    cache=False,
)

target(
    name="intg_test",
    run="heph query --include //test/... | heph query --include test - | heph run -",
    tools=['heph'],
    pass_env=["PATH"],
    sandbox=False,
    cache=False,
)

go_env_vars = ["GOROOT", "GOPATH", "HOME"]

target(
    name="go_test",
    run="go test -v ./...",
    tools=["go"],
    cache=False,
    sandbox=False,
    pass_env=go_env_vars,
)

target(
    name="build_all",
    run="heph query --include build | heph run -",
    tools=['heph'],
    pass_env=go_env_vars,
    sandbox=False,
    cache=False,
)

builds = []

version = target(
    name="version",
    run="echo ${GITHUB_SHA::7} > version && cat version",
    out="version",
    pass_env=["GITHUB_SHA"],
    cache=False,
)

extra_src = [
    "cmd/root_usage_template.gotpl",
    "engine/predeclared.gotpl",
    version,
]

for os in ["linux", "darwin"]:
    for arch in ["amd64", "arm64"]:
        name = "heph_{}_{}".format(os, arch)
        t = target(
            name="build_{}_{}".format(os, arch),
            run=[
                "mv version utils/version",
                "pwd && go version && CGO_ENABLED=0 go build -tags release -ldflags='-s -w' -o {} .".format(name)
            ],
            out=name,
            deps=["go.mod", "go.sum"] + glob("**/*.go") + extra_src,
            env={
                "GOOS": os,
                "GOARCH": arch,
            },
            tools=["go"],
            labels=["build"],
            pass_env=go_env_vars,
        )
        builds.append(t)

target(
    name="cp_builds",
    run="cp * $0",
    deps=[builds],
    cache=False,
    pass_args=True,
)
