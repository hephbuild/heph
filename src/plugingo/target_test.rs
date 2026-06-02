use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::htvalue::Value;
use crate::plugingo::addr_util::{import_path_to_dep_group, write_importcfg_script};
use crate::plugingo::factors::Factors;
use std::collections::{BTreeMap, HashMap};

pub struct GoEnv<'a> {
    pub gomodcache: &'a str,
    pub gopath: &'a str,
    pub gocache: &'a str,
}

/// Compile the test package (GoFiles + TestGoFiles) with `go tool compile` in test mode.
///
/// Output: `<pkgname>_test.a`
#[expect(clippy::too_many_arguments, reason = "all parameters are required")]
pub fn build_test_lib_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: &[String],
    test_src_addrs: &[String],
    go_bin_addr: &str,
    goroot: &str,
    gocache: &str,
    embed_addr: Option<&Addr>,
    embed_file_addrs: &[String],
) -> TargetSpec {
    let out_file = format!("{}_test.a", package_name);
    let run = compile_run_script(
        import_path,
        transitive_libs,
        &out_file,
        embed_addr.is_some(),
    );

    build_lib_spec_inner(
        addr,
        factors,
        transitive_libs,
        src_addrs.iter().chain(test_src_addrs.iter()).cloned(),
        go_bin_addr,
        goroot,
        gocache,
        embed_addr,
        embed_file_addrs,
        run,
        out_file,
    )
}

/// Compile the external test package (XTestGoFiles) with `go tool compile`.
///
/// Output: `<pkgname>_xtest.a`
#[expect(clippy::too_many_arguments, reason = "all parameters are required")]
pub fn build_xtest_lib_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    xtest_src_addrs: &[String],
    go_bin_addr: &str,
    goroot: &str,
    gocache: &str,
    embed_addr: Option<&Addr>,
    embed_file_addrs: &[String],
) -> TargetSpec {
    // xtest package import path is "<import_path>_test"
    let xtest_import_path = format!("{}_test", import_path);
    let out_file = format!("{}_xtest.a", package_name);
    let run = compile_run_script(
        &xtest_import_path,
        transitive_libs,
        &out_file,
        embed_addr.is_some(),
    );

    build_lib_spec_inner(
        addr,
        factors,
        transitive_libs,
        xtest_src_addrs.iter().cloned(),
        go_bin_addr,
        goroot,
        gocache,
        embed_addr,
        embed_file_addrs,
        run,
        out_file,
    )
}

/// Compile the generated testmain.go with `go tool compile`.
///
/// Output: `testmain_main.a`
pub fn build_testmain_lib_spec(
    addr: Addr,
    factors: &Factors,
    testmain_src_addr: &Addr,
    testmain_libs: &[(String, Addr)],
    go_bin_addr: &str,
    goroot: &str,
    gocache: &str,
) -> TargetSpec {
    let out_file = "testmain_main.a".to_string();
    let run = compile_run_script("main", testmain_libs, &out_file, false);

    build_lib_spec_inner(
        addr,
        factors,
        testmain_libs,
        std::iter::once(testmain_src_addr.format()),
        go_bin_addr,
        goroot,
        gocache,
        None,
        &[],
        run,
        out_file,
    )
}

/// Link the test binary with `go tool link`.
///
/// Depends on the testmain lib (as main package) plus a flat list of lib
/// archives — one per importpath needed by the link. Caller is responsible
/// for constructing `all_libs` so each importpath appears exactly once.
///
/// Internal vs xtest are linked as SEPARATE binaries (heph-legacy mirror) so
/// each importcfg is constraint-free:
/// - internal bin: P → build_test_lib, transitive(P.imports ∪ P.test_imports) → build_lib
/// - xtest bin:    P → build_lib (normal), P_test → build_xtest_lib, transitive(P.xtest_imports ∪ P.imports) → build_lib
///
/// Mixing both in one bin requires per-test recompile of every transitive
/// importer of P (Option X) — much higher cache pressure. The split avoids
/// it because Go itself rejects internal-test cycles (so internal bin can't
/// have a transitive importer of P), and xtest's compile uses P=normal (so
/// the xtest cycle stays fingerprint-consistent).
///
/// Output: `test_binary`
pub fn build_test_spec(
    addr: Addr,
    factors: &Factors,
    go_bin_addr: &str,
    go_env: &GoEnv<'_>,
    testmain_lib_addr: &Addr,
    all_libs: &[(String, Addr)],
) -> TargetSpec {
    let mut script = write_importcfg_script(all_libs, None);
    script.push_str(
        "\"$SRC_GO_BIN\" tool link -importcfg \"$importcfg\" -buildmode=pie -o test_binary \"$SRC_TESTMAIN\"\n",
    );

    let mut deps: BTreeMap<String, Value> = BTreeMap::new();
    deps.insert(
        "go_bin".to_string(),
        Value::List(vec![Value::String(go_bin_addr.to_string())]),
    );
    deps.insert(
        "testmain".to_string(),
        Value::List(vec![Value::String(testmain_lib_addr.format())]),
    );
    for (import_path, lib_addr) in all_libs {
        let group = import_path_to_dep_group(import_path);
        deps.insert(group, Value::List(vec![Value::String(lib_addr.format())]));
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), Value::String(script));
    config.insert("deps".to_string(), Value::Map(deps.into_iter().collect()));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "bin".to_string(),
            Value::List(vec![Value::String("test_binary".to_string())]),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        Value::Map(HashMap::from([
            ("GOOS".to_string(), Value::String(factors.goos.clone())),
            ("GOARCH".to_string(), Value::String(factors.goarch.clone())),
            (
                "GOMODCACHE".to_string(),
                Value::String(go_env.gomodcache.to_string()),
            ),
            (
                "GOPATH".to_string(),
                Value::String(go_env.gopath.to_string()),
            ),
            (
                "GOCACHE".to_string(),
                Value::String(go_env.gocache.to_string()),
            ),
        ])),
    );
    // CGO pin lives in `env` (hashed) so stale CGO=1 archives don't survive
    // cache lookups (pluginexec/mod.rs:70 excludes runtime_env from the def hash).
    config.insert(
        "env".to_string(),
        Value::Map(HashMap::from([(
            "CGO_ENABLED".to_string(),
            Value::String("0".to_string()),
        )])),
    );
    config.insert(
        "pass_env".to_string(),
        Value::List(vec![
            Value::String("GOROOT".to_string()),
            Value::String("GOMODCACHE".to_string()),
            Value::String("GOPATH".to_string()),
        ]),
    );

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

/// The `test` target: runs the linked test binary.
///
/// `data_query_addr` is a `@heph/query` addr selecting sibling targets labeled
/// `go_test_data`. The engine expands the query lazily, so this provider does
/// not need to scan the package itself.
pub fn test_spec(addr: Addr, build_test_addr: Addr, data_query_addr: &Addr) -> TargetSpec {
    // exec driver passes argv literally — no shell expansion. The engine
    // stages dep outputs at <pkg>/<file> under ws_dir, and the process cwd
    // is the target's package dir (engine/driver_managed_os.rs:78). The
    // `test` target is a sibling of `build_test` in the same package, so
    // its `test_binary` output lands next to us and `./test_binary`
    // resolves regardless of package depth.
    let run = vec![
        Value::String("./test_binary".to_string()),
        Value::String("-test.v".to_string()),
    ];

    let deps_map: HashMap<String, Value> = HashMap::from([
        (
            "bin".to_string(),
            Value::List(vec![Value::String(build_test_addr.format())]),
        ),
        (
            "data".to_string(),
            Value::List(vec![Value::String(data_query_addr.format())]),
        ),
    ]);

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), Value::List(run));
    config.insert("deps".to_string(), Value::Map(deps_map));
    config.insert("out".to_string(), Value::Map(HashMap::new()));
    TargetSpec {
        addr,
        driver: "exec".to_string(),
        config,
        labels: vec!["test".to_string(), "go-test".to_string()],
        transitive: Default::default(),
    }
}

// ---- helpers ----

fn compile_run_script(
    p_flag: &str,
    transitive_libs: &[(String, Addr)],
    out_file: &str,
    has_embed: bool,
) -> String {
    let mut script = write_importcfg_script(transitive_libs, None);
    let embedcfg_flag = if has_embed {
        " -embedcfg \"$SRC_EMBED\""
    } else {
        ""
    };

    script.push_str(&format!(
        "\"$SRC_GO_BIN\" tool compile -p \"{p_flag}\" -trimpath=\"$WORKSPACE_ROOT\" -pack -importcfg \"$importcfg\"{embedcfg_flag} -shared -o \"{out_file}\" \"@${{LIST_SRC}}\"\n",
    ));

    script
}

#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required, no natural grouping"
)]
fn build_lib_spec_inner(
    addr: Addr,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: impl Iterator<Item = String>,
    go_bin_addr: &str,
    goroot: &str,
    gocache: &str,
    embed_addr: Option<&Addr>,
    embed_file_addrs: &[String],
    run: String,
    out_file: String,
) -> TargetSpec {
    let mut deps: BTreeMap<String, Value> = transitive_libs
        .iter()
        .map(|(import_path, dep_addr)| {
            let group = import_path_to_dep_group(import_path);
            (group, Value::List(vec![Value::String(dep_addr.format())]))
        })
        .collect();

    deps.insert(
        String::new(),
        Value::List(src_addrs.map(Value::String).collect()),
    );
    deps.insert(
        "go_bin".to_string(),
        Value::List(vec![Value::String(go_bin_addr.to_string())]),
    );
    if let Some(e) = embed_addr {
        deps.insert(
            "embed".to_string(),
            Value::List(vec![Value::String(e.format())]),
        );
    }
    // Embed files travel as inputs so the engine stages them into the sandbox
    // alongside sources — embedcfg paths are pkg-relative and must resolve here.
    if !embed_file_addrs.is_empty() {
        deps.insert(
            "embed_files".to_string(),
            Value::List(
                embed_file_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), Value::String(run));
    config.insert("deps".to_string(), Value::Map(deps.into_iter().collect()));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "a".to_string(),
            Value::List(vec![Value::String(out_file)]),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        Value::Map(HashMap::from([
            ("GOOS".to_string(), Value::String(factors.goos.clone())),
            ("GOARCH".to_string(), Value::String(factors.goarch.clone())),
            ("GOROOT".to_string(), Value::String(goroot.to_string())),
            ("GOCACHE".to_string(), Value::String(gocache.to_string())),
        ])),
    );
    // CGO pin lives in `env` (hashed) so stale CGO=1 archives don't survive
    // cache lookups (pluginexec/mod.rs:70 excludes runtime_env from the def hash).
    config.insert(
        "env".to_string(),
        Value::Map(HashMap::from([(
            "CGO_ENABLED".to_string(),
            Value::String("0".to_string()),
        )])),
    );
    config.insert(
        "pass_env".to_string(),
        Value::List(vec![Value::String("GOROOT".to_string())]),
    );

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec!["go-build".to_string()],
        transitive: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;
    use crate::pluginfs;

    fn mk_addr(pkg: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(pkg), name.to_string(), Default::default())
    }

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        }
    }

    fn go_env() -> GoEnv<'static> {
        GoEnv {
            gomodcache: "/go/pkg/mod",
            gopath: "/go",
            gocache: "/tmp/gocache",
        }
    }

    fn src_addrs(pkg: &str) -> Vec<String> {
        vec![pluginfs::file_addr(&format!("{}/foo.go", pkg)).format()]
    }

    // ---- build_test_lib_spec ----

    #[test]
    fn test_build_test_lib_spec_driver_is_bash() {
        let spec = build_test_lib_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_build_test_lib_spec_out_is_test_a() {
        let spec = build_test_lib_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        let files = match out.get("a").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        let fname = match &files[0] {
            Value::String(s) => s,
            _ => panic!("expected string"),
        };
        assert!(
            fname.ends_with("_test.a"),
            "test lib output must end with _test.a: {}",
            fname
        );
    }

    #[test]
    fn test_build_test_lib_spec_run_uses_compile() {
        let spec = build_test_lib_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("tool compile"),
            "run must use go tool compile: {run}"
        );
        assert!(
            !run.contains("test -c"),
            "run must NOT use go test -c: {run}"
        );
    }

    // ---- build_xtest_lib_spec ----

    #[test]
    fn test_build_xtest_lib_spec_out_is_xtest_a() {
        let spec = build_xtest_lib_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        let files = match out.get("a").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        let fname = match &files[0] {
            Value::String(s) => s,
            _ => panic!("expected string"),
        };
        assert!(
            fname.ends_with("_xtest.a"),
            "xtest lib output must end with _xtest.a: {}",
            fname
        );
    }

    #[test]
    fn test_build_xtest_lib_spec_uses_xtest_import_path() {
        let spec = build_xtest_lib_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("example.com/pkg_test"),
            "xtest compile must use _test import path: {run}"
        );
    }

    // ---- build_testmain_lib_spec ----

    #[test]
    fn test_build_testmain_lib_spec_out_is_testmain_main_a() {
        let testmain_src = pluginfs::file_addr("pkg/testmain.go");
        let spec = build_testmain_lib_spec(
            mk_addr("pkg", "build_testmain_lib"),
            &test_factors(),
            &testmain_src,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        let files = match out.get("a").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        let fname = match &files[0] {
            Value::String(s) => s,
            _ => panic!("expected string"),
        };
        assert_eq!(fname, "testmain_main.a");
    }

    #[test]
    fn test_build_testmain_lib_spec_uses_p_main() {
        let testmain_src = pluginfs::file_addr("pkg/testmain.go");
        let spec = build_testmain_lib_spec(
            mk_addr("pkg", "build_testmain_lib"),
            &test_factors(),
            &testmain_src,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("-p \"main\""),
            "testmain lib must use -p main: {run}"
        );
    }

    // ---- build_test_spec ----

    #[test]
    fn test_build_test_spec_driver_is_bash() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            &testmain_lib,
            &[],
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_build_test_spec_out_has_bin_group() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            &testmain_lib,
            &[],
        );
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            out.contains_key("bin"),
            "build_test out must have 'bin' group"
        );
    }

    #[test]
    fn test_build_test_spec_run_uses_tool_link() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            &testmain_lib,
            &[],
        );
        let run = match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("tool link"),
            "build_test run must use go tool link: {run}"
        );
        assert!(
            !run.contains("test -c"),
            "build_test run must NOT use go test -c: {run}"
        );
    }

    #[test]
    fn test_build_test_spec_env_pins_cgo_disabled() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            &testmain_lib,
            &[],
        );
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"),
            "link env must pin CGO_ENABLED=0 in the hashed map: {:?}",
            env.get("CGO_ENABLED")
        );
    }

    #[test]
    fn test_build_test_lib_spec_env_pins_cgo_disabled() {
        let spec = build_test_lib_spec(
            mk_addr("pkg", "build_test_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"),
            "compile env must pin CGO_ENABLED=0 in the hashed map: {:?}",
            env.get("CGO_ENABLED")
        );
    }

    #[test]
    fn test_build_test_spec_deps_has_testmain() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            &testmain_lib,
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("testmain"),
            "deps must have testmain group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_build_test_spec_deps_has_go_bin() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            "//@heph/bin:go",
            &go_env(),
            &testmain_lib,
            &[],
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("go_bin"),
            "deps must have go_bin group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    // ---- test_spec ----

    fn query_addr(pkg: &str) -> Addr {
        crate::htaddr::parse_addr(&format!(
            "//{}:q@package={},label=go_test_data",
            crate::pluginquery::PACKAGE,
            pkg,
        ))
        .expect("parse query addr")
    }

    #[test]
    fn test_test_spec_deps_on_build_test() {
        let build_addr = mk_addr("mypkg", "build_test");
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            build_addr.clone(),
            &query_addr("mypkg"),
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m.clone(),
            _ => panic!(),
        };
        assert!(deps.contains_key("bin"));
        let bin_dep = match deps.get("bin").unwrap() {
            Value::List(v) => v.clone(),
            _ => panic!(),
        };
        assert!(matches!(&bin_dep[0], Value::String(s) if s.contains("build_test")));
    }

    #[test]
    fn test_test_spec_driver_is_exec() {
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            mk_addr("mypkg", "build_test"),
            &query_addr("mypkg"),
        );
        assert_eq!(spec.driver, "exec");
        let run = match spec.config.get("run").expect("run present") {
            Value::List(v) => v.clone(),
            other => panic!("run must be a list for exec driver: {other:?}"),
        };
        let argv: Vec<String> = run
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                other => panic!("argv element must be string: {other:?}"),
            })
            .collect();
        assert_eq!(
            argv,
            vec!["./test_binary".to_string(), "-test.v".to_string()]
        );
    }

    #[test]
    fn test_build_test_lib_spec_has_go_build_label() {
        let spec = build_test_lib_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        assert!(
            spec.labels.contains(&"go-build".to_string()),
            "build_test_lib spec must carry go-build label: {:?}",
            spec.labels
        );
    }

    #[test]
    fn test_build_xtest_lib_spec_has_go_build_label() {
        let spec = build_xtest_lib_spec(
            mk_addr("pkg", "build_xtest_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
            None,
            &[],
        );
        assert!(
            spec.labels.contains(&"go-build".to_string()),
            "build_xtest_lib spec must carry go-build label: {:?}",
            spec.labels
        );
    }

    #[test]
    fn test_build_testmain_lib_spec_has_go_build_label() {
        let spec = build_testmain_lib_spec(
            mk_addr("pkg", "build_testmain_lib"),
            &test_factors(),
            &mk_addr("pkg", "_testmain_src"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        assert!(
            spec.labels.contains(&"go-build".to_string()),
            "build_testmain_lib spec must carry go-build label: {:?}",
            spec.labels
        );
    }

    #[test]
    fn test_test_spec_has_test_labels() {
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            mk_addr("mypkg", "build_test"),
            &query_addr("mypkg"),
        );
        assert!(spec.labels.contains(&"test".to_string()));
        assert!(spec.labels.contains(&"go-test".to_string()));
    }

    #[test]
    fn test_test_spec_data_group_is_query_addr() {
        let qa = query_addr("mypkg");
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            mk_addr("mypkg", "build_test"),
            &qa,
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m.clone(),
            _ => panic!(),
        };
        let data_dep = match deps.get("data").expect("data group present") {
            Value::List(v) => v.clone(),
            _ => panic!("data group must be a list"),
        };
        assert_eq!(data_dep.len(), 1);
        let s = match &data_dep[0] {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            s.contains("@heph/query"),
            "data dep must be query addr: {s}"
        );
        assert!(
            s.contains("label=go_test_data"),
            "must filter by label: {s}"
        );
        assert!(s.contains("package=mypkg"), "must filter by package: {s}");
        assert_eq!(s, qa.format());
    }
}
