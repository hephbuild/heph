cfg = CONFIG["go_backend"]

go = cfg["go"]
if not go:
    fail("set go_backend.go")

gofmt = cfg["gofmt"]
if not gofmt:
    fail("set go_backend.gofmt")

go_version = cfg["version"]
if not go_version:
    fail("set go_backend.go_version")

sedi = target(
    name="sedi",
    deps=["sedi.sh"],
    out='sedi.sh',
)

target(
    name="go",
    run="./go_install.sh '{}'".format(go_version),
    deps=["go_install.sh", "go.sh"],
    tools=[sedi],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
    },
    out={
        "go": "go.sh",
        "gofmt": "go/bin/gofmt",
    },
    cache=["go", "go.sh"],
    provide={
        "OUTDIR": "$(outdir)",
    },
)

godeps = target(
    name="_godeps#build",
    deps=glob("**/*.go")+["go.mod"],
    run="go build -o godeps .",
    out={"godeps": "godeps"},
    tools=[go],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
    },
)

generate_testmain = target(
    name="_generate_testmain#build",
    deps=glob("generate_testmain/**/*.go")+["generate_testmain/go.mod"],
    run="cd generate_testmain && go build -o generate_testmain .",
    out={"generate_testmain": "generate_testmain/generate_testmain"},
    tools=[go],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
    },
)

def go_install(name, pkg, version, bin_name):
    return target(
        name=name,
        run="export GOBIN=$(pwd)/gobin && go install {}@{}".format(pkg, version),
        tools=[go],
        out="gobin/"+bin_name,
        env={
            "OS": get_os(),
            "ARCH": get_arch(),
        },
    )

def go_mod(mod_pkgs=[], test={}):
    gomodsum = group(
        name="_mod",
        deps=glob("go.mod")+glob("go.sum"),
    )

    src = group(
        name="_src",
        deps=[
            gomodsum,
            '$(collect "{}/..." include="go_src")'.format(package_fqn()),
            '$(collect "{}/..." include="go_test_src")'.format(package_fqn()),
        ]+glob("**/*.go"),
    )

    mod_pkg_srcs = []
    for pkg in mod_pkgs:
        mod_pkg_srcs.append(pkg+":_src")

    godeps_cfg = {
        'test': test,
        'go': go,
        'godeps': godeps,
        'generate_testmain': generate_testmain,
    }

    target(
        name="_go_mod_gen",
        run="godeps \"$TARGET\" mod '{}'".format(to_json(godeps_cfg)),
        out="/**/BUILD.gen",
        deps=[src]+mod_pkg_srcs,
        tools=[go, godeps],
        env={
            "OS": get_os(),
            "ARCH": get_arch(),
        },
        gen=True,
    )

    target(
        name="mod_tidy",
        labels=["mod_tidy"],
        run='GOPATH="$SANDBOX/thirdparty/go/gopath" go mod tidy -v',
        tools=[go],
        deps=[
            gomodsum,
            '$(collect {}/... include="go_pkg")'.format(package_fqn()),
            '$(collect {}/... include="go_pkg_test")'.format(package_fqn()),
        ],
        sandbox=False,
        cache=False,
    )

    target(
        name="fmt",
        labels=["fmt"],
        run="go fmt ./...",
        tools=[go],
        deps=[src],
        sandbox=False,
        cache=False,
    )

    target(
        name="go_vet",
        labels=["go_vet"],
        run="go vet ./...",
        tools=[go],
        deps=[src],
        sandbox=False,
        cache=False,
    )

def go_bin(name, pkg, build_deps=[], *args, **kwargs):
    return
    out = "_out_"+name

    pkg_path = ""
    if pkg:
        pkg_path = "/"+pkg

    build = target(
        name="_"+name+"#build",
        run=['GOPATH="$SANDBOX/thirdparty/go/gopath" go build -mod=readonly -o {} .{}'.format(out, pkg_path)],
        deps=[
                 package_fqn()+pkg_path+':_go_pkg',
                 '$(find_parent ":_mod")',
                 ]+build_deps,
        tools=[go],
        out={name: out},
        env={
            "OS": get_os(),
            "ARCH": get_arch(),
        },
        require_gen=True,
    )

    return target(
        name=name,
        tools=[build],
        run=name,
        sandbox=False,
        cache=False,
        pass_args=True,
        require_gen=True,
        *args, **kwargs,
    )
