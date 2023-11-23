cfg = CONFIG["go_backend"]

go = cfg["go"]
if not go:
    fail("set go_backend.go")

gofmt = cfg["gofmt"]
if not gofmt:
    fail("set go_backend.gofmt")

go_toolchain_installsh = group(
    name = "_go_toolchain_installsh",
    deps = ["go_install.sh"],
)

go_toolchain_gosh = group(
    name = "_go_toolchain_gosh",
    deps = ["go.sh"],
)

def go_toolchain(name, version, architectures = []):
    run = [
        "./$SRC_INSTALL '{}'".format(version),
        "mv $SRC_GO $OUT_GO",
        "export GO_OUTDIR=$(pwd)",
        "export GODEBUG='installgoroot=all'",
    ]

    for arch in architectures:
        (goos, _, goarch) = arch.partition("_")
        run.append(
            "(export GOOS={} && export GOARCH={} && ./$OUT_GO install --trimpath std)".format(
                goos,
                goarch,
            ),
        )

    return target(
        name = name,
        run = run,
        deps = {
            "install": go_toolchain_installsh,
            "go": go_toolchain_gosh,
        },
        env = {
            "OS": get_os(),
            "ARCH": get_arch(),
        },
        out = {
            "go": "go.sh",
            "gofmt": "go/bin/gofmt",
        },
        cache = heph.cache(history = 1),
        support_files = "go",
        transitive = heph.target_spec(runtime_env = {"GO_OUTDIR": "$(outdir)"}),
    )

godeps = target(
    name = "_godeps#build",
    deps = glob("godeps/**/*.go") + ["godeps/go.mod"],
    run = "cd godeps && go build -o godeps .",
    out = {"godeps": "godeps/godeps"},
    tools = [go],
    transitive = heph.target_spec(tools = go),
    env = {
        "GOWORK": "off",
        "OS": get_os(),
        "ARCH": get_arch(),
    },
)

generate_testmain = target(
    name = "_generate_testmain#build",
    deps = glob("generate_testmain/**/*.go") + ["generate_testmain/go.mod"],
    run = "cd generate_testmain && go build -o generate_testmain .",
    out = {"generate_testmain": "generate_testmain/generate_testmain"},
    tools = [go],
    env = {
        "GOWORK": "off",
        "OS": get_os(),
        "ARCH": get_arch(),
    },
)

backend_dir = heph.pkg.dir()

backend_pkg = heph.pkg.addr()

def go_install(name, pkg, version, bin_name):
    return target(
        name = name,
        run = [
            "export GOBIN=$(pwd)/gobin",
            "go install -modcacherw {}@{}".format(pkg, version),
        ],
        tools = [go],
        out = "gobin/" + bin_name,
        env = {
            "OS": get_os(),
            "ARCH": get_arch(),
        },
    )

def go_mod_gen_addr(pkg):
    return heph.canonicalize(pkg + ":_go_mod_gen")

def _find_go_work():
    parts = heph.path.split(heph.pkg.dir())

    prefix = "."
    for i in range(0, len(parts) + 1):
        p = prefix + "/go.work"

        if len(glob(p)) > 0:
            return p

        prefix += "/.."

    fail("unable to locate work file from " + heph.pkg.dir())

def go_mod(
    mod_pkgs = [],
    cfg = {},
    replace = {},
    work = False,
):
    goenv = {}
    if not work:
        goenv["GOWORK"] = "off"

    gomodsum = group(
        name = "_mod",
        deps = ["go.mod"] + glob("go.sum"),
    )

    src = group(
        name = "_src",
        deps = [
            gomodsum,
            '$(collect "{}/..." include="go_src")'.format(heph.pkg.addr()),
            '$(collect "{}/..." include="go_test_src")'.format(heph.pkg.addr()),
        ] + glob("**/*.go"),
    )

    mod_pkg_modsums = []
    for pkg in mod_pkgs:
        mod_pkg_modsums.append(pkg + ":_mod")

    mod_pkg_srcs = []
    for pkg in mod_pkgs:
        mod_pkg_srcs.append(pkg + ":_src")

    godeps_cfg = json_file(
        name = "godeps_cfg",
        data = {
            "pkg": cfg,
            "replace": replace,
            "go": go,
            "generate_testmain": generate_testmain,
            "backend_pkg": backend_pkg,
        },
    )

    godeps_deps = [gomodsum, src] + mod_pkg_srcs + mod_pkg_modsums

    if work:
        p = _find_go_work()
        godeps_deps += [p]
        godeps_deps += glob(p + ".sum")

    imports = target(
        name = "_go_mod_gen_imports",
        run = "godeps imports > $OUT",
        out = "imports",
        deps = godeps_deps,
        tools = godeps,
        gen_deps_meta = True,
        env = goenv,
    )

    (pkg, _, _) = heph.split(go)

    mod_pkgs_gen = [
        "//" + pkg + ":_std_pkgs_*",
        "go_lib",
        "go_build_bin",
        "test",
        "thirdparty",
        "go-test",
        "//thirdparty/go/**:_go_mod_download*",
    ]

    for pkg in mod_pkgs + ["//thirdparty/go"]:
        mod_pkgs_gen += [pkg + "/**:_go_lib*"]

    # Add labels defined in cfg
    for pkg in cfg.values():
        if "test" in pkg:
            if "run" in pkg["test"]:
                if "labels" in pkg["test"]["run"]:
                    labels = pkg["test"]["run"]["labels"]
                    if type(labels) != "list":
                        labels = [labels]

                    mod_pkgs_gen += labels

    mod_pkgs_gen += [
        heph.pkg.addr() + "/**:_go_lib*",
        heph.pkg.addr() + "/**:go_bin#build*",
        heph.pkg.addr() + "/**:_go_test_lib*",
        heph.pkg.addr() + "/**:_go_xtest_lib*",
        heph.pkg.addr() + "/**:_go_testmain_lib*",
        heph.pkg.addr() + "/**:_go_xtestmain_lib*",
        heph.pkg.addr() + "/**:_go_gen_testmain*",
        heph.pkg.addr() + "/**:_go_gen_xtestmain*",
        heph.pkg.addr() + "/**:_go_test#build*",
        heph.pkg.addr() + "/**:_go_xtest#build*",
        heph.pkg.addr() + "/**:go_test*",
        heph.pkg.addr() + "/**:go_xtest*",
    ]

    target(
        name = "_go_mod_gen",
        run = "godeps mod $SRC_CFG",
        out = "/**/BUILD",
        deps = {
            "cfg": godeps_cfg,
        },
        runtime_deps = {
            "deps": godeps_deps,
        },
        hash_deps = [imports, gomodsum],
        tools = godeps,
        env = {
            "GOOS": get_os(),
            "GOARCH": get_arch(),
        } | goenv,
        gen = mod_pkgs_gen,
        gen_deps_meta = True,
    )

    target(
        name = "mod_tidy",
        labels = ["mod_tidy"],
        run = "go mod tidy -v",
        tools = [go],
        deps = [src],
        env = goenv,
        sandbox = False,
        cache = False,
    )

    target(
        name = "fmt",
        labels = ["fmt"],
        run = "go fmt ./...",
        tools = [go],
        deps = [src],
        env = goenv,
        sandbox = False,
        cache = False,
    )

    target(
        name = "go_vet",
        labels = ["go_vet"],
        run = "go vet ./...",
        tools = [go],
        deps = [src],
        env = goenv,
        sandbox = False,
        cache = False,
    )

def _go_gen_importcfg():
    xargs = "xargs " + ("-S" if get_os() == "darwin" else "-s") + " 100000"

    return [
        'cat "$SRC_STD" | {} -I[] echo "packagefile []=$GO_OUTDIR/go/pkg/${{GOOS}}_${{GOARCH}}/[].a" | sort -u > $SANDBOX/importconfig'.format(xargs),
        'echo "" >> $SANDBOX/importconfig',
        'find "$SANDBOX" -name "*.importcfg" | {} -I[] cat [] | sed -e "s:=:=$SANDBOX/:" | sort -u >> $SANDBOX/importconfig'.format(xargs),
    ]

def _go_compile_cmd(name, import_path, abi, complete, embed_cfg):
    extra = ""
    if abi:
        extra += " -symabis $SRC_ABI -asmhdr $SRC_ABI_H"

    if embed_cfg:
        extra += " -embedcfg $SRC_EMBED"

    if complete:
        extra += " -complete"

    extra += " -p " + import_path

    return [
        'echo "Compiling..."',
        'go tool compile -importcfg $SANDBOX/importconfig -trimpath "$ROOT;$GO_OUTDIR" -o $SANDBOX/$OUT_A -pack{} $SRC_SRC'.format(extra),
        'echo "packagefile {}=$OUT_A" > $SANDBOX/$OUT_IMPORTCFG'.format(import_path),
    ]

def go_mod_download(name, path, version):
    return target(
        name = name,
        run = [
            "echo module heph_ignore > go.mod", # stops go reading the main go.mod, and downloading all of those too
            "go mod download -modcacherw -json {}@{} | tee mod.json".format(path, version),
            "rm go.mod",
            """export MOD_DIR=$(cat mod.json | awk -F\\" '/"Dir": / { print $4 }') && cp -r "$MOD_DIR/." .""",
        ],
        tools = [go],
        out = ["."],
        labels = ["thirdparty"],
        env = {"GOWORK": "off"},
    )

def go_library(
    name,
    import_path,
    go_files,
    complete = True,
    libs = [],
    s_files = [],
    resources = [],
    abi = None,
    os = get_os(),
    arch = get_arch(),
    tags = [],
    dir = None,
    src_dep = None,
    gen_embed = False,
):
    pvt_name = name if name[0] == "_" else "_" + name

    p = dir + "/" if dir else ""

    if not import_path:
        fail("import_path cannot be empty")

    pkg_name = import_path.rpartition("/")[-1]

    if not pkg_name:
        fail("pkg_name cannot be empty")

    if type(src_dep) != "list":
        src_dep = [src_dep]

    if dir:
        s_files = [p + dep for dep in s_files]

    if len(s_files) > 0:
        abi = target(
            name = pvt_name + "#abi",
            deps = src_dep if src_dep else s_files,
            run = [
                "eval $(go env)",
                "touch $OUT_H", # the go Toolchain does this
                'go tool asm -I . -I $GOROOT/pkg/include -trimpath "$ROOT;$GO_OUTDIR" -D GOOS_$GOOS -D GOARCH_$GOARCH -p {} -gensymabis -o "$OUT_SYMABIS" $SRC'.format(import_path),
            ],
            tools = [go],
            out = {
                "symabis": p + "symabis",
                "h": p + "go_asm.h",
            },
            env = {
                "GOOS": os,
                "GOARCH": arch,
                "SRC": " ".join(s_files),
            },
        )
        lib = go_library(
            name = pvt_name + "#lib",
            import_path = import_path,
            src_dep = src_dep,
            complete = False,
            abi = abi,
            libs = libs,
            go_files = go_files,
            resources = resources,
            dir = dir,
            os = os,
            arch = arch,
            tags = tags,
        )

        asm = []
        for s_file in s_files:
            t = target(
                name = pvt_name + "#asm_" + heph.path.base(s_file),
                deps = {
                    "lib": lib + "|a",
                    "hdr": lib + "|h",
                    "_": src_dep if src_dep else s_file,
                },
                run = [
                    "eval $(go env)",
                    'go tool asm -I . -I $GOROOT/pkg/include -trimpath "$ROOT;$GO_OUTDIR" -D GOOS_$GOOS -D GOARCH_$GOARCH -p {} -o "$OUT" $SRC_ASM'.format(import_path),
                ],
                tools = [go],
                out = p + s_file.replace(".s", ".o"),
                env = {
                    "GOOS": os,
                    "GOARCH": arch,
                    "SRC_ASM": s_file,
                },
            )
            asm.append(t)

        return target(
            name = name,
            tools = [go],
            deps = {
                "lib": lib + "|a",
                "asm": asm,
            },
            run = [
                'go tool pack r "$SRC_LIB" $SRC_ASM',
                'echo "packagefile {}=$OUT_A" > $SANDBOX/$OUT_IMPORTCFG'.format(import_path),
            ],
            out_env = "rel_root",
            out = {
                "a": p + pkg_name + ".a",
                "importcfg": p + pkg_name + ".importcfg",
            },
            env = {
                "GOOS": os,
                "GOARCH": arch,
            },
            labels = ["go_lib"],
        )

    env = {
        "GOOS": os,
        "GOARCH": arch,
    }

    src_files = go_files
    if dir:
        src_files = [p + dep for dep in src_files]
        resources = [p + dep for dep in resources]

    if src_dep:
        deps = {
            "src": src_dep,
            "libs": libs,
            "res": resources,
        }
        env["SRC_SRC"] = " ".join(src_files)
        env["SRC_RES"] = " ".join(resources)
    else:
        deps = {
            "src": src_files,
            "libs": libs,
            "res": resources,
        }
        deps["src"].append('$(collect "{}/." include="go_src")'.format(heph.pkg.addr()))

    if len(resources) > 0 or gen_embed:
        embed_deps = deps | {} # copy deps
        embed_deps["libs"] = [] # remove libs from deps

        embed_cfg = target(
            name = pvt_name + "#embedcfg",
            run = "godeps embed $SRC_SRC > $OUT",
            deps = embed_deps,
            out = p + pkg_name + ".embedcfg",
            tools = [go, godeps],
            env = env,
            gen_deps_meta = True,
        )
    else:
        embed_cfg = None

    deps = deps | {
        "std": _std_pkgs(os, arch),
        "embed": embed_cfg,
    }

    out = {
        "a": p + pkg_name + ".a",
        "importcfg": p + pkg_name + ".importcfg",
    }
    if abi:
        deps["abi"] = abi + "|symabis"
        deps["abi_h"] = abi + "|h"
        out["h"] = "go_asm.h"

    return target(
        name = name,
        deps = deps,
        run = _go_gen_importcfg() + _go_compile_cmd(name, import_path, abi, complete, embed_cfg),
        out = out,
        out_env = "rel_root",
        tools = [go],
        env = env,
        labels = ["go_lib"],
    )

def _std_pkgs(os, arch):
    (pkg, _, _) = heph.split(go)

    return target(
        name = "_std_pkgs_{}_{}".format(os, arch),
        pkg = "//" + pkg,
        tools = [go],
        run = "export CGO_ENABLED=1 && go list std > std_list",
        out = "std_list",
        env = {
            "GOOS": os,
            "GOARCH": arch,
        },
    )

def go_build_bin(
    name,
    main,
    libs = [],
    out = None,
    os = get_os(),
    arch = get_arch(),
    tags = [],
    flags = "",
    deps = {},
    runtime_deps = None,
):
    if not out:
        out = heph.pkg.name()

    (_, _, output) = heph.split(main)
    if not output:
        main = main + "|a"

    _deps = {
        "libs": libs,
        "main": main,
        "std": _std_pkgs(os, arch),
    }

    return target(
        name = name,
        doc = "Build {} {}/{} {}".format(heph.pkg.dir(), os, arch, " ".join(tags)).strip(),
        deps = _deps | deps,
        runtime_deps = runtime_deps,
        run = _go_gen_importcfg() + [
            'go tool link -importcfg "$SANDBOX/importconfig" -o $SANDBOX/$OUT {} $SRC_MAIN'.format(
                flags,
            ),
        ],
        out_env = "rel_root",
        tools = [go],
        labels = ["go_build_bin"],
        out = out,
        env = {
            "GOOS": os,
            "GOARCH": arch,
        },
    )

def go_bin_build_addr_name(
    name = None,
    os = get_os(),
    arch = get_arch(),
    tags = [],
):
    if not name:
        name = "os={},arch={}".format(os, arch)
        if len(tags) > 0:
            name += ",tags={" + ",".join(tags) + "}"

    return "go_bin#build@" + name

def go_bin_build_addr(
    pkg = None,
    name = None,
    os = get_os(),
    arch = get_arch(),
    tags = [],
):
    if not pkg:
        pkg = heph.pkg.addr()

    return heph.canonicalize(
        pkg + ":" + go_bin_build_addr_name(name = name, os = os, arch = arch, tags = tags),
    )

def go_bin(
    name,
    pkg = None,
    variant_name = None,
    os = get_os(),
    arch = get_arch(),
    tags = [],
    *args,
    **kwargs,
):
    if not pkg:
        pkg = heph.pkg.addr()

    kwargs = {
        "name": name,
        "tools": {
            "bin": go_bin_build_addr(
                pkg = pkg,
                name = variant_name,
                os = os,
                arch = arch,
                tags = tags,
            ),
        },
        "sandbox": False,
        "cache": False,
        "pass_args": True,
    } | kwargs

    if "run" not in kwargs and "entrypoint" not in kwargs:
        kwargs = {
            "run": "$TOOL_BIN",
            "entrypoint": "exec",
        } | kwargs

    return target(*args, **kwargs)
