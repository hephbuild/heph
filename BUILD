load("//backend/go", "go_toolchain")
load("//backend/node", "node_toolchain")
load("//backend/node", "yarn_toolchain")

go_toolchain(
    name="go",
    version="1.18.4",
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
    name="e2e_test",
    run="heph query --include e2e_test | heph run -",
    tools=['heph'],
    pass_env=["PATH"],
    sandbox=False,
    cache=False,
)

target(
    name="e2e_isolated_test",
    run="heph query --include e2e_isolated | heph run -",
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

extra_src = [
    "engine/predeclared.gotpl",
]
deps = ["go.mod", "go.sum"] + glob("**/*.go", exclude=["website", "backend", "test"]) + extra_src

release = "release" in CONFIG["profiles"]

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
        out = "heph_{}_{}".format(os, arch)
        t = target(
            name="build_{}_{}".format(os, arch),
            run=[
                "go version",
                "pwd",
                "go build -trimpath {} -ldflags='-s -w' -o $OUT .".format(build_flags)
            ],
            out=out,
            deps=deps,
            env={
                "GOOS": os,
                "GOARCH": arch,
                "CGO_ENABLED": "1",
            },
            tools=["go"],
            labels=["build"],
            pass_env=go_env_vars,
        )
        builds.append(t)

        target(
            name="docker_build_{}_{}".format(os, arch),
            deps=deps,
            run=[
                'docker run --platform=$GOOS/$GOARCH --privileged --cap-add=ALL --rm -it --volume=$(pwd):/ws --volume=/tmp/gocache:/gocache --volume=/tmp/gopath:/gopath --workdir=/ws -e GOCACHE=/gocache -e GOPATH=/gopath -e GOOS=$GOOS -e GOARCH=$GOARCH -e CGO_ENABLED=$CGO_ENABLED golang:1.19 bash -c "go build -trimpath {} -ldflags="-s -w" -o $OUT ."'.format(build_flags),
                'mv heph $OUT',
            ],
            env={
                "GOOS": os,
                "GOARCH": arch,
                "CGO_ENABLED": "1",
            },
            out=out,
            tools='docker',
        )

target(
    name="linux",
    deps=[
        '//:build_linux_amd64',
    ],
    run=[
        'docker run --privileged --cap-add=ALL --rm -it --volume=$(dirname $SRC):/ws --volume=/Users/XXX/Downloads/py3:/tmp/py3 --workdir=/ws ubuntu ./$(basename $SRC) "$@"',
    ],
    tools='docker',
    src_env='abs',
    pass_args=True,
    cache=False,
)

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
