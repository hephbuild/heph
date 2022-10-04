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
    runtime_env={
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
backend_dir = heph.pkg.dir()
backend_pkg = heph.pkg.addr()

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

def go_mod_gen_addr(pkg):
    return heph.canonicalize(pkg+":_go_mod_gen")

def go_mod(mod_pkgs=[], cfg={}, sandbox=True):
    gomodsum = group(
        name="_mod",
        deps=glob("go.mod")+glob("go.sum"),
    )

    src = group(
        name="_src",
        deps=[
            gomodsum,
            '$(collect "{}/..." include="go_src")'.format(heph.pkg.addr()),
            '$(collect "{}/..." include="go_test_src")'.format(heph.pkg.addr()),
        ]+glob("**/*.go"),
    )

    mod_pkg_modsums = []
    for pkg in mod_pkgs:
        mod_pkg_modsums.append(pkg+":_mod")

    mod_pkg_srcs = []
    for pkg in mod_pkgs:
        mod_pkg_srcs.append(pkg+":_src")

    godeps_cfg = json_file(name="godeps_cfg", data={
        'pkg': cfg,
        'go': go,
        'godeps': godeps,
        'generate_testmain': generate_testmain,
        'std_pkgs_target': std_pkgs,
        'std_pkgs_list_file': backend_dir+"/std_list",
        'backend_pkg': backend_pkg,
    })

    godeps_deps = [src]+mod_pkg_srcs+mod_pkg_modsums

    imports = target(
        name="_go_mod_gen_imports",
        run="godeps imports > $OUT",
        out="imports",
        deps=godeps_deps,
        tools=[go, godeps],
    )

    mod_gen_kwargs={}
    if not sandbox:
        mod_gen_kwargs={
            "sandbox": False,
            "out_in_sandbox": True
        }

    target(
        name="_go_mod_gen",
        run="godeps mod $SRC_CFG",
        out="/**/BUILD",
        deps={'deps': godeps_deps, 'cfg': godeps_cfg},
        hash_deps=[imports, gomodsum, godeps_cfg],
        tools=[go, godeps],
        env={
            "GOOS": get_os(),
            "GOARCH": get_arch(),
        },
        gen=True,
        **mod_gen_kwargs,
    )

    target(
        name="mod_tidy",
        labels=["mod_tidy"],
        run='GOPATH="$SANDBOX/thirdparty/go/gopath" go mod tidy -v',
        tools=[go],
        deps=[src],
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

def go_work():
    group(
        name="_go_work",
        deps="go.work",
    )

def _go_gen_importcfg():
    xargs = "xargs " + ("-S" if get_os() == "darwin" else "-s")+" 100000"
    std_file = backend_dir+"/std_list"

    return [
        'echo "Gen importconfig..."',
        'echo "std: $SANDBOX/{}"'.format(std_file),
        'cat "$SANDBOX/{}" | {} -I[] echo "packagefile []=$GO_OUTDIR/go/pkg/${{OS}}_${{ARCH}}/[].a" | sort -u > $SANDBOX/importconfig'.format(std_file, xargs),
        'echo "" >> $SANDBOX/importconfig',
        'find "$SANDBOX" -name "*.importcfg" | {} -I[] cat [] | sed -e "s:=:=$SANDBOX/:" | sort -u >> $SANDBOX/importconfig'.format(xargs),
    ]

def _go_compile_cmd(name, import_path, abi, complete, embed_cfg):
    extra = ""
    if abi:
        extra += ' -symabis $SRC_ABI -asmhdr "$SANDBOX/$OUT_H"'

    if embed_cfg:
        extra += ' -embedcfg $SRC_EMBED'

    if complete:
        extra += ' -complete'

    return [
        'echo "Compiling (out: $OUT_IMPORTCFG)..."',
        'go tool compile -importcfg $SANDBOX/importconfig -trimpath "$ROOT;$GO_OUTDIR" -o $SANDBOX/$OUT_A -pack{} $SRC_SRC'.format(extra),
        'echo "packagefile {}=$OUT_A" > $SANDBOX/$OUT_IMPORTCFG'.format(import_path),
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
        labels=['thirdparty'],
    )

def go_library(name, import_path, go_files, complete=True, libs=[], s_files=[], resources=[], abi=None, dir=None, src_dep=None, gen_embed=False):
    p = dir+"/" if dir else ""

    if not import_path:
        fail("import_path cannot be empty")

    pkg_name=import_path.rpartition("/")[-1]

    if not pkg_name:
        fail("pkg_name cannot be empty")

    if dir:
        s_files = [p+dep for dep in s_files]

    if len(s_files) > 0:
        abi = target(
            name="_"+name+"#abi",
            deps=src_dep if src_dep else s_files,
            run='eval `go env` && go tool asm -I $GOROOT/pkg/include -trimpath "$ROOT;$GO_OUTDIR" -D GOOS_$OS -D GOARCH_$ARCH -p {} -gensymabis -o "$OUT" $SRC'.format(import_path),
            tools=[go],
            out=p+'lib.abi',
            env={
                "OS": get_os(),
                "ARCH": get_arch(),
                "SRC": ' '.join(s_files)
            },
        )
        lib = go_library(
            name="_"+name+"#lib",
            import_path=import_path,
            src_dep=src_dep,
            complete=False,
            abi=abi,
            libs=libs,
            go_files=go_files,
            resources=resources,
            dir=dir,
        )
        asm = target(
            name="_"+name+"#asm",
            deps={
                'lib': lib+"|a",
                'hdr': lib+"|h",
                'asm': src_dep if src_dep else s_files,
            },
            run='eval `go env` && go tool asm -I $GOROOT/pkg/include -trimpath "$ROOT;$GO_OUTDIR" -D GOOS_$GOOS -D GOARCH_$GOARCH -p {} -o "$OUT" $SRC_ASM'.format(import_path),
            tools=[go],
            out=p+'asm.o',
            env={
                "OS": get_os(),
                "ARCH": get_arch(),
                "SRC_ASM": ' '.join(s_files)
            },
        )
        return target(
            name=name,
            tools=[go],
            deps={
                'lib': lib+"|a",
                'asm': asm,
            },
            run=[
                'go tool pack r "$SRC_LIB" $SRC_ASM',
                'echo "packagefile {}=$OUT_A" > $SANDBOX/$OUT_IMPORTCFG'.format(import_path),
            ],
            out_env='rel_root',
            out={'a': p+pkg_name+'.a', 'importcfg': p+pkg_name+'.importcfg'},
            env={
                "OS": get_os(),
                "ARCH": get_arch(),
            },
            labels=['go_lib'],
        )

    env = {
        "OS": get_os(),
        "ARCH": get_arch(),
    }

    src_files = go_files
    if dir:
        src_files = [p+dep for dep in src_files]
        resources = [p+dep for dep in resources]

    if src_dep:
        deps = {'src': [src_dep], 'libs': libs}
        env["SRC_SRC"] = ' '.join(src_files)
        env["SRC_RES"] = ' '.join(resources)
    else:
        deps = {'src': src_files, 'libs': libs, 'res': resources}

    deps['src'].append('$(collect "{}/." include="go_src")'.format(heph.pkg.addr()))

    if len(resources) > 0 or gen_embed:
        embed_deps = deps | {}
        embed_deps['libs'] = []

        embed_cfg = target(
            name="_"+name+"#embedcfg",
            run='godeps embed $SRC_SRC > $OUT',
            deps=embed_deps,
            out=p+pkg_name+'.embedcfg',
            tools=[go, godeps],
            env=env,
        )
    else:
        embed_cfg = None

    deps = deps | {
        'std': std_pkgs,
        'embed': embed_cfg,
    }

    out = {'a': p+pkg_name+'.a', 'importcfg': p+pkg_name+'.importcfg'}
    if abi:
        deps["abi"] = abi
        out["h"] = 'lib.h'

    return target(
        name=name,
        deps=deps,
        run=_go_gen_importcfg()+_go_compile_cmd(name, import_path, abi, complete, embed_cfg),
        out=out,
        out_env='rel_root',
        tools=[go],
        env=env,
        labels=['go_lib'],
    )

def go_build_bin(name, main, libs=[], out=None):
    if not out:
        out = heph.pkg.name()

    _, _, output = heph.split(main)
    if not output:
        main = main+"|a"

    return target(
        name=name,
        deps={
            'libs': libs,
            'main': main,
            'std': std_pkgs,
        },
        run=_go_gen_importcfg()+['go tool link -importcfg "$SANDBOX/importconfig" -o $SANDBOX/$OUT $SRC_MAIN'],
        out_env='rel_root',
        tools=[go],
        labels=['go_build_bin'],
        out=out,
        env={
            "OS": get_os(),
            "ARCH": get_arch(),
        },
    )

def go_bin_build_addr(pkg):
    return heph.canonicalize(pkg+":go_bin#build")

def go_bin(name, pkg=None, *args, **kwargs):
    if not pkg:
        pkg = heph.pkg.addr()

    kwargs = {
        "name": name,
        "tools": go_bin_build_addr(pkg),
        "sandbox": False,
        "cache": False,
        "pass_args": True,
    } | kwargs

    if "run" not in kwargs and "executor" not in kwargs:
        kwargs = {
            "run": "$TOOL_GO_BIN_BUILD",
            "executor": "exec",
        } | kwargs

    return target(
        *args, **kwargs,
    )
