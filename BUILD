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
    name="light_e2e_test",
    run="heph query --include light_e2e_test | heph run -",
    tools=['heph'],
    pass_env=["PATH"],
    sandbox=False,
    cache=False,
)

target(
    name="e2e_test",
    run="heph query --include e2e | heph run -",
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

def _gobuild(pkg, args):
    return "go build -trimpath {} -ldflags='-s -w' -o $OUT {}".format(build_flags, pkg)

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
                _gobuild('heph/cmd/heph', build_flags),
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
            name="build_lwe_{}_{}".format(os, arch),
            run=[
                _gobuild('heph/cmd/heph', build_flags),
            ],
            out="lwe_{}_{}".format(os, arch),
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

def docker_run(args="", volumes=[], env=[]):
    volumes+=['$(pwd)']
    env+=['PATH=$(pwd):/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin']
    env=[e if ('=' in e) else e+'=$'+e for e in env]

    volumes_flags = ['--volume={}:{}'.format(p, p) for p in volumes]
    env_flags = ['-e '+e for e in env]

    flags=' '.join(volumes_flags+env_flags)
    flags+=' --workdir=$(pwd)'

    return 'docker run --privileged --cap-add=ALL --rm -t {} {}'.format(flags, args)

target(
    name="docker",
    deps={
        'heph': '//:build_linux_amd64',
        'lwe': '//:build_lwe_linux_amd64',
    },
    run=[
        'mv $SRC_HEPH heph',
        'mv $SRC_LWE lwe',
        docker_run('"$@"'),
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
