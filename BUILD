load("//backend/go", "go_toolchain")
load("//backend/node", "node_toolchain")
load("//backend/node", "yarn_toolchain")

go_toolchain(
    name="go",
    version="1.20.1",
    architectures=[
        "darwin_amd64", "darwin_arm64",
        "linux_amd64", "linux_arm64",
    ],
)

node = node_toolchain(
    name="node",
    version="v16.15.1",
)

yarn_toolchain(
    name="yarn",
    version="v1.22.19",
    node=node,
)

target(
    name="test_light_e2e",
    run="heph query --include light_e2e | heph run -",
    tools=['heph'],
    pass_env=["PATH", "TERM"],
    sandbox=False,
    cache=False,
)

target(
    name="test_e2e",
    run="heph query --include e2e | heph run -",
    tools=['heph'],
    pass_env=["PATH", "TERM"],
    sandbox=False,
    cache=False,
)

target(
    name="test_intg",
    run="heph query --include //test/... | heph query --include test - | heph run -",
    tools=['heph'],
    pass_env=["PATH", "TERM"],
    sandbox=False,
    cache=False,
)

go_env_vars = ["GOROOT", "GOPATH", "HOME"]

target(
    name="test_go",
    run="CGO_ENABLED=0 go test -v ./...",
    tools=["go"],
    cache=False,
    sandbox=False,
    pass_env="*",
)

target(
    name="build_all",
    run="heph query --include build | heph run -",
    tools=['heph'],
    pass_env="*",
    sandbox=False,
    cache=False,
)

extra_src = [
    'hbuiltin/predeclared.gotpl',
    'platform/initfile.sh',
]
deps = ["go.mod", "go.sum"] +\
       glob("**/*.go", exclude=["website", "backend", "test"]) +\
       extra_src

release = "release" in CONFIG["profiles"]

def _gobuild(pkg, args):
    return "go build -trimpath {} -ldflags='-s -w' -o $OUT {}".format(args, pkg)

build_flags=""
if release:
    version = target(
        name="version",
        run="echo ${GITHUB_SHA::7} > $OUT",
        out="utils/version",
        pass_env=["GITHUB_SHA"],
    )
    deps.append(version)

    build_flags = "-tags release"

builds = []
for os in ["linux", "darwin"]:
    for arch in ["amd64", "arm64"]:
        t = target(
            name="build_{}_{}".format(os, arch),
            run=[
                "go version",
                _gobuild('github.com/hephbuild/heph/cmd/heph', build_flags),
            ],
            out="heph_{}_{}".format(os, arch),
            deps=deps,
            env={
                "GOOS": os,
                "GOARCH": arch,
                "CGO_ENABLED": "0",
            },
            tools=["go"],
            labels=["build"],
            pass_env=go_env_vars,
        )
        builds.append(t)

target(
    name="cp_builds",
    run="cp * $1",
    deps=[builds],
    cache=False,
    pass_args=True,
)

target(
    name="start-jaeger",
    run="""
    docker run -d --name jaeger \
      -e COLLECTOR_ZIPKIN_HTTP_PORT=9411 \
      -p 5775:5775/udp \
      -p 6831:6831/udp \
      -p 6832:6832/udp \
      -p 5778:5778 \
      -p 16686:16686 \
      -p 14268:14268 \
      -p 9411:9411 \
      jaegertracing/all-in-one:1.39
      """,
    cache=False,
    pass_env="*",
)
