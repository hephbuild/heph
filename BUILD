target(
    name="test",
    run="$HEPH query --include e2e_test | $HEPH run -",
    pass_env=["PATH"],
)

go_env_vars = ["GOROOT", "GOPATH", "HOME"]

target(
    name="build_all",
    run="$HEPH query --include build | $HEPH run -",
    pass_env=go_env_vars,
)

builds = []

extra_src = [
    "cmd/root_usage_template.gotpl"
]

version = target(
    name="version",
    run="echo ${GITHUB_SHA::8} > version && cat version",
    out="version",
    pass_env=["GITHUB_SHA"],
    sandbox=True,
)

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
            deps=["go.mod", "go.sum"] + glob("**/*.go") + extra_src + [version],
            env={
                "GOOS": os,
                "GOARCH": arch,
            },
            tools=["go"],
            labels=["build"],
            pass_env=go_env_vars,
            sandbox=True,
            cache=True,
        )
        builds.append(t)

target(
    name="cp_builds",
    run="cp * $1",
    deps=[builds],
    sandbox=True,
    pass_args=True,
)
