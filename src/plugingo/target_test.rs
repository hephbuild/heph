use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
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
pub fn build_lib_test_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: &[Addr],
    test_src_addrs: &[Addr],
    go_bin_addr: &str,
    goroot: &str,
    embed_addr: Option<&Addr>,
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
        src_addrs.iter().chain(test_src_addrs.iter()),
        go_bin_addr,
        goroot,
        embed_addr,
        run,
        out_file,
    )
}

/// Compile the external test package (XTestGoFiles) with `go tool compile`.
///
/// Output: `<pkgname>_xtest.a`
#[expect(clippy::too_many_arguments, reason = "all parameters are required")]
pub fn build_lib_xtest_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    xtest_src_addrs: &[Addr],
    go_bin_addr: &str,
    goroot: &str,
    embed_addr: Option<&Addr>,
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
        xtest_src_addrs.iter(),
        go_bin_addr,
        goroot,
        embed_addr,
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
) -> TargetSpec {
    let out_file = "testmain_main.a".to_string();
    let run = compile_run_script("main", testmain_libs, &out_file, false);

    build_lib_spec_inner(
        addr,
        factors,
        testmain_libs,
        std::iter::once(testmain_src_addr),
        go_bin_addr,
        goroot,
        None,
        run,
        out_file,
    )
}

/// Link the test binary with `go tool link`.
///
/// Depends on the `build_testmain_lib` output (as main package) plus all transitive
/// lib archives for importcfg generation.
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
        "\"$SRC_GO_BIN\" tool link -importcfg \"$importcfg\" -o test_binary \"$SRC_TESTMAIN\"\n",
    );

    // Build deps map: testmain lib gets its own group, all transitive libs get lib_* groups
    let mut deps: BTreeMap<String, TargetSpecValue> = BTreeMap::new();
    deps.insert(
        "go_bin".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
    );
    deps.insert(
        "testmain".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(testmain_lib_addr.format())]),
    );
    for (import_path, lib_addr) in all_libs {
        let group = import_path_to_dep_group(import_path);
        deps.insert(
            group,
            TargetSpecValue::List(vec![TargetSpecValue::String(lib_addr.format())]),
        );
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(script));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(deps.into_iter().collect()),
    );
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "bin".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("test_binary".to_string())]),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        TargetSpecValue::Map(HashMap::from([
            (
                "GOOS".to_string(),
                TargetSpecValue::String(factors.goos.clone()),
            ),
            (
                "GOARCH".to_string(),
                TargetSpecValue::String(factors.goarch.clone()),
            ),
            (
                "GOMODCACHE".to_string(),
                TargetSpecValue::String(go_env.gomodcache.to_string()),
            ),
            (
                "GOPATH".to_string(),
                TargetSpecValue::String(go_env.gopath.to_string()),
            ),
            (
                "GOCACHE".to_string(),
                TargetSpecValue::String(go_env.gocache.to_string()),
            ),
        ])),
    );
    config.insert(
        "pass_env".to_string(),
        TargetSpecValue::List(vec![
            TargetSpecValue::String("GOROOT".to_string()),
            TargetSpecValue::String("GOMODCACHE".to_string()),
            TargetSpecValue::String("GOPATH".to_string()),
        ]),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

/// The `test` target: runs the linked test binary.
pub fn test_spec(addr: Addr, build_test_addr: Addr) -> TargetSpec {
    let run = "\"$SRC_BIN\" -test.v".to_string();

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "bin".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(build_test_addr.format())]),
        )])),
    );
    config.insert("out".to_string(), TargetSpecValue::Map(HashMap::new()));
    TargetSpec {
        addr,
        driver: "bash".to_string(),
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
        "\"$SRC_GO_BIN\" tool compile -p \"{p_flag}\" -trimpath -pack -importcfg \"$importcfg\"{embedcfg_flag} -o \"{out_file}\" \"@${{LIST_SRC}}\"\n",
    ));

    script
}

#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required, no natural grouping"
)]
fn build_lib_spec_inner<'a>(
    addr: Addr,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: impl Iterator<Item = &'a Addr>,
    go_bin_addr: &str,
    goroot: &str,
    embed_addr: Option<&Addr>,
    run: String,
    out_file: String,
) -> TargetSpec {
    let mut deps: BTreeMap<String, TargetSpecValue> = transitive_libs
        .iter()
        .map(|(import_path, dep_addr)| {
            let group = import_path_to_dep_group(import_path);
            (
                group,
                TargetSpecValue::List(vec![TargetSpecValue::String(dep_addr.format())]),
            )
        })
        .collect();

    deps.insert(
        String::new(),
        TargetSpecValue::List(
            src_addrs
                .map(|a| TargetSpecValue::String(a.format()))
                .collect(),
        ),
    );
    deps.insert(
        "go_bin".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
    );
    if let Some(e) = embed_addr {
        deps.insert(
            "embed".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(e.format())]),
        );
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("run".to_string(), TargetSpecValue::String(run));
    config.insert(
        "deps".to_string(),
        TargetSpecValue::Map(deps.into_iter().collect()),
    );
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "a".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String(out_file)]),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        TargetSpecValue::Map(HashMap::from([
            (
                "GOOS".to_string(),
                TargetSpecValue::String(factors.goos.clone()),
            ),
            (
                "GOARCH".to_string(),
                TargetSpecValue::String(factors.goarch.clone()),
            ),
            (
                "GOROOT".to_string(),
                TargetSpecValue::String(goroot.to_string()),
            ),
        ])),
    );
    config.insert(
        "pass_env".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String("GOROOT".to_string())]),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec![],
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

    fn src_addrs(pkg: &str) -> Vec<Addr> {
        vec![pluginfs::file_addr(&format!("{}/foo.go", pkg))]
    }

    // ---- build_lib_test_spec ----

    #[test]
    fn test_build_lib_test_spec_driver_is_bash() {
        let spec = build_lib_test_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_build_lib_test_spec_out_is_test_a() {
        let spec = build_lib_test_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        let files = match out.get("a").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        let fname = match &files[0] {
            TargetSpecValue::String(s) => s,
            _ => panic!("expected string"),
        };
        assert!(
            fname.ends_with("_test.a"),
            "test lib output must end with _test.a: {}",
            fname
        );
    }

    #[test]
    fn test_build_lib_test_spec_run_uses_compile() {
        let spec = build_lib_test_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
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

    // ---- build_lib_xtest_spec ----

    #[test]
    fn test_build_lib_xtest_spec_out_is_xtest_a() {
        let spec = build_lib_xtest_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        let files = match out.get("a").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        let fname = match &files[0] {
            TargetSpecValue::String(s) => s,
            _ => panic!("expected string"),
        };
        assert!(
            fname.ends_with("_xtest.a"),
            "xtest lib output must end with _xtest.a: {}",
            fname
        );
    }

    #[test]
    fn test_build_lib_xtest_spec_uses_xtest_import_path() {
        let spec = build_lib_xtest_spec(
            mk_addr("pkg", "build_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            "//@heph/bin:go",
            "/usr/local/go",
            None,
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
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
        );
        let out = match spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        let files = match out.get("a").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        let fname = match &files[0] {
            TargetSpecValue::String(s) => s,
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
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
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
        assert_eq!(spec.driver, "bash");
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
            TargetSpecValue::Map(m) => m,
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
            TargetSpecValue::String(s) => s.clone(),
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
            TargetSpecValue::Map(m) => m,
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
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("go_bin"),
            "deps must have go_bin group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    // ---- test_spec ----

    #[test]
    fn test_test_spec_deps_on_build_test() {
        let build_addr = mk_addr("mypkg", "build_test");
        let spec = test_spec(mk_addr("mypkg", "test"), build_addr.clone());
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m.clone(),
            _ => panic!(),
        };
        assert!(deps.contains_key("bin"));
        let bin_dep = match deps.get("bin").unwrap() {
            TargetSpecValue::List(v) => v.clone(),
            _ => panic!(),
        };
        assert!(matches!(&bin_dep[0], TargetSpecValue::String(s) if s.contains("build_test")));
    }

    #[test]
    fn test_test_spec_has_test_labels() {
        let spec = test_spec(mk_addr("mypkg", "test"), mk_addr("mypkg", "build_test"));
        assert!(spec.labels.contains(&"test".to_string()));
        assert!(spec.labels.contains(&"go-test".to_string()));
    }
}
