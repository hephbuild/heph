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
    deps=glob("godeps/**/*.go")+["godeps/go.mod"],
    run="cd godeps && go build -o godeps .",
    out={"godeps": "godeps/godeps"},
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

std_pkgs = target(
    name="_std_pkgs",
    tools=[go],
    run='export CGO_ENABLED=1 && go list std > std_list',
    out="std_list",
)
backend_dir = package_dir()
backend_pkg = package_fqn()

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
        'std_pkgs_target': std_pkgs,
        'std_pkgs_list_file': backend_dir+"/std_list",
        'backend_pkg': backend_pkg,
    }

    target(
        name="_go_mod_gen",
        run="godeps \"$TARGET\" mod '{}'".format(to_json(godeps_cfg)),
        out="/**/BUILD",
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
def go_gen_importcfg():
    xargs = "xargs " + ("-S" if get_os() == "darwin" else "-s")+" 100000"
    std_file = backend_dir+"/std_list"

    return [
        'echo "Gen importconfig..."',
        'echo "std: $SANDBOX/{}"'.format(std_file),
        'cat "$SANDBOX/{}" | {} -I[] echo "packagefile []=$GO_OUTDIR/go/pkg/${{OS}}_${{ARCH}}/[].a" | sort -u > $SANDBOX/importconfig'.format(std_file, xargs),
        'find "$SANDBOX" -name "importcfg" | {} -I[] cat [] | sed -e "s:=:=$SANDBOX/:" | sort -u >> $SANDBOX/importconfig'.format(xargs),
    ]

def go_compile_cmd(import_path, files, dir=None):
    cd = ('cd '+dir+' && ' if dir else '')
    dir = dir+"/" if dir else ""

    return [
        'echo "Compiling ({})..."'.format(dir),
        cd + 'go tool compile -importcfg $SANDBOX/importconfig -trimpath "$ROOT" -o lib.a -pack '+files,
        'echo "packagefile {}=$PACKAGE/lib.a" > {}importcfg'.format(import_path, dir),
    ]

def go_mod_download(name, path, version):
    return target(
        name=name,
        run=[
            'echo module heph_ignore > go.mod', # stops go reading the main go.mod, and downloading all of those too
            "go mod download -modcacherw -json {}@{} | tee mod.json".format(path, version),
            'rm go.mod',
            'export MOD_DIR=$(cat mod.json | awk -F\\" \'/"Dir": / { print $4 }\') && cp -r "$MOD_DIR/." .',
        ],
        tools=[go],
        out=["."],
    )

def go_library(name, import_path, deps, files, dir=None):
    p = dir+"/" if dir else ""

    return target(
        name=name,
        deps=deps,
        run=go_gen_importcfg()+go_compile_cmd(import_path, files, dir),
        out=[p+'lib.a', p+'importcfg'],
        tools=[go],
        env={
            "OS": get_os(),
            "ARCH": get_arch(),
        },
    )

def go_bin_link(name, path, deps, out):
    return target(
        name=name,
        deps=deps,
        run=go_gen_importcfg()+['go tool link -importcfg "$SANDBOX/importconfig" -o {} {}'.format(out, path)],
        tools=[go],
        out=out,
        env={
            "OS": get_os(),
            "ARCH": get_arch(),
        },
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
