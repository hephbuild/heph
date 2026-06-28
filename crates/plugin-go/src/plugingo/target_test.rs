use crate::plugingo::addr_util::{
    go_build_env, go_host_pass_env_config, go_run_prelude, go_sdk_dep, go_sdk_read_only_config,
    import_path_to_dep_group, to_run_value, write_importcfg_script,
};
use crate::plugingo::driver_compile::{CompileParams, build_compile_spec};
use crate::plugingo::factors::Factors;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::{BTreeMap, HashMap};

/// Extra environment configuration plumbed onto the `test`/`xtest` run targets
/// from a package's `provider_state(provider="go", test={...})`. Mirrors the
/// exec driver's env knobs: `env`/`pass_env` are hashed (affect cache),
/// `runtime_env`/`runtime_pass_env` are runtime-only (not hashed).
#[derive(Debug, Default, Clone)]
pub struct TestEnv {
    pub env: BTreeMap<String, String>,
    pub runtime_env: BTreeMap<String, String>,
    pub pass_env: Vec<String>,
    pub runtime_pass_env: Vec<String>,
    /// Shell lines run before the test binary. When non-empty the `test`/`xtest`
    /// target switches from the `exec` driver to the `bash` driver.
    pub pre_run: Vec<String>,
}

/// Shared `go_compile` builder for the test/xtest/testmain lib compiles.
/// `golist_addr` is `Some` iff the variant embeds; the driver resolves the
/// embedcfg in-process (no separate `go_embed` target).
#[expect(clippy::too_many_arguments, reason = "all parameters are required")]
fn compile_lib(
    addr: Addr,
    p_flag: String,
    out_file: String,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: Vec<String>,
    golist_addr: Option<&Addr>,
    embed_variant: &str,
    embed_file_addrs: &[String],
    embed_src_addrs: &[String],
    go_version: &str,
) -> TargetSpec {
    build_compile_spec(CompileParams {
        addr,
        p_flag,
        out_file,
        factors,
        go_version,
        transitive_libs,
        src_addrs: &src_addrs,
        s_files: &[],
        s_file_addrs: &[],
        hdr_addrs: &[],
        golist_addr,
        embed_variant: if golist_addr.is_some() {
            embed_variant
        } else {
            ""
        },
        embed_file_addrs,
        embed_src_addrs,
    })
}

/// Compile the test package (GoFiles + TestGoFiles), test mode. Output:
/// `<pkgname>_test.a`.
#[expect(clippy::too_many_arguments, reason = "all parameters are required")]
pub fn build_test_lib_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: &[String],
    test_src_addrs: &[String],
    golist_addr: Option<&Addr>,
    embed_file_addrs: &[String],
    embed_src_addrs: &[String],
    go_version: &str,
) -> TargetSpec {
    let src: Vec<String> = src_addrs
        .iter()
        .chain(test_src_addrs.iter())
        .cloned()
        .collect();
    compile_lib(
        addr,
        import_path.to_string(),
        format!("{package_name}_test.a"),
        factors,
        transitive_libs,
        src,
        golist_addr,
        "test_embed",
        embed_file_addrs,
        embed_src_addrs,
        go_version,
    )
}

/// Compile the external test package (XTestGoFiles). Output: `<pkgname>_xtest.a`.
#[expect(clippy::too_many_arguments, reason = "all parameters are required")]
pub fn build_xtest_lib_spec(
    addr: Addr,
    import_path: &str,
    package_name: &str,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    xtest_src_addrs: &[String],
    golist_addr: Option<&Addr>,
    embed_file_addrs: &[String],
    embed_src_addrs: &[String],
    go_version: &str,
) -> TargetSpec {
    compile_lib(
        addr,
        format!("{import_path}_test"),
        format!("{package_name}_xtest.a"),
        factors,
        transitive_libs,
        xtest_src_addrs.to_vec(),
        golist_addr,
        "xtest_embed",
        embed_file_addrs,
        embed_src_addrs,
        go_version,
    )
}

/// Compile the generated testmain.go. Output: `testmain_main.a`.
pub fn build_testmain_lib_spec(
    addr: Addr,
    factors: &Factors,
    testmain_src_addr: &Addr,
    testmain_libs: &[(String, Addr)],
    go_version: &str,
) -> TargetSpec {
    compile_lib(
        addr,
        "main".to_string(),
        "testmain_main.a".to_string(),
        factors,
        testmain_libs,
        vec![testmain_src_addr.format()],
        None,
        "",
        &[],
        &[],
        go_version,
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
    testmain_lib_addr: &Addr,
    all_libs: &[(String, Addr)],
    go_version: &str,
) -> TargetSpec {
    let mut script = go_run_prelude(go_version);
    script.extend(write_importcfg_script(all_libs, None));
    script.push(
        "\"$GO\" tool link -importcfg \"$importcfg\" -buildmode=pie -o test_binary \"$SRC_TESTMAIN\"".to_string(),
    );

    let mut deps: BTreeMap<String, Value> = BTreeMap::new();
    deps.insert(
        "testmain".to_string(),
        Value::List(vec![Value::String(testmain_lib_addr.format())]),
    );
    for (import_path, lib_addr) in all_libs {
        let group = import_path_to_dep_group(import_path);
        deps.insert(group, Value::List(vec![Value::String(lib_addr.format())]));
    }
    if let Some((sdk_group, sdk_val)) = go_sdk_dep(go_version) {
        deps.insert(sdk_group, sdk_val);
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(script));
    config.insert("deps".to_string(), Value::Map(deps.into_iter().collect()));
    if let Some((ro_k, ro_v)) = go_sdk_read_only_config(go_version) {
        config.insert(ro_k, ro_v);
    }
    if let Some((pe_k, pe_v)) = go_host_pass_env_config(go_version) {
        config.insert(pe_k, pe_v);
    }
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
        ])),
    );
    // CGO/toolchain pins live in `env` (hashed) so stale archives don't survive
    // cache lookups (pluginexec/mod.rs:70 excludes runtime_env from the def hash).
    config.insert("env".to_string(), go_build_env());

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        ..Default::default()
    }
}

/// The `test` target: runs the linked test binary.
///
/// `data_query_addr` is a `@heph/query` addr selecting sibling targets labeled
/// `go_test_data`. The engine expands the query lazily, so this provider does
/// not need to scan the package itself.
///
/// `test_env.pre_run` carries shell lines from the package's
/// `test = {"pre_run": [...]}` provider state. When empty, the target uses the
/// `exec` driver and runs the test binary as a literal argv. When non-empty, it
/// switches to the `bash` driver so the `pre_run` lines execute (as shell)
/// before the test binary.
pub fn test_spec(
    addr: Addr,
    build_test_addr: Addr,
    data_query_addr: &Addr,
    test_env: &TestEnv,
) -> TargetSpec {
    // Both drivers run with cwd = the target's package dir, and the engine
    // stages dep outputs at <pkg>/<file> under ws_dir
    // (engine/driver_managed_os.rs:78). The `test` target is a sibling of
    // `build_test` in the same package, so its `test_binary` output lands next
    // to us and `./test_binary` resolves regardless of package depth.
    let (driver, run) = if test_env.pre_run.is_empty() {
        // exec driver passes argv literally — no shell expansion.
        let run = Value::List(vec![
            Value::String("./test_binary".to_string()),
            Value::String("-test.v".to_string()),
        ]);
        ("exec", run)
    } else {
        // bash driver: each list element is one shell line, joined with `\n`.
        // Run the user's `pre_run` lines first, then the test binary.
        let mut lines: Vec<String> = test_env.pre_run.clone();
        lines.push("./test_binary -test.v".to_string());
        ("bash", to_run_value(lines))
    };

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
    config.insert("run".to_string(), run);
    config.insert("deps".to_string(), Value::Map(deps_map));
    config.insert("out".to_string(), Value::Map(HashMap::new()));

    // Plumb package-scoped test env from provider_state. Omitted keys keep the
    // spec minimal so cache fingerprints are unchanged for the common case.
    if !test_env.env.is_empty() {
        config.insert("env".to_string(), str_map_value(&test_env.env));
    }
    if !test_env.runtime_env.is_empty() {
        config.insert(
            "runtime_env".to_string(),
            str_map_value(&test_env.runtime_env),
        );
    }
    if !test_env.pass_env.is_empty() {
        config.insert("pass_env".to_string(), str_list_value(&test_env.pass_env));
    }
    if !test_env.runtime_pass_env.is_empty() {
        config.insert(
            "runtime_pass_env".to_string(),
            str_list_value(&test_env.runtime_pass_env),
        );
    }

    TargetSpec {
        addr,
        driver: driver.to_string(),
        config,
        labels: vec!["test".to_string(), "go-test".to_string()],
        ..Default::default()
    }
}

// ---- helpers ----

fn str_map_value(m: &BTreeMap<String, String>) -> Value {
    Value::Map(
        m.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

fn str_list_value(v: &[String]) -> Value {
    Value::List(v.iter().cloned().map(Value::String).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbuiltins::pluginfs;
    use hmodel::htpkg::PkgBuf;

    /// Go version under test.
    const V: &str = crate::plugingo::toolchain::DEFAULT_GO_VERSION;

    fn run_str(spec: &TargetSpec) -> String {
        match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            Value::List(v) => v
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.as_str(),
                    _ => panic!("run entry not a string"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("run not string or list"),
        }
    }

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
            None,
            &[],
            &[],
            V,
        );
        assert_eq!(spec.driver, "go_compile");
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
            None,
            &[],
            &[],
            V,
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
            None,
            &[],
            &[],
            V,
        );
        assert_eq!(spec.driver, "go_compile");
        assert_eq!(
            crate::plugingo::driver_compile::test_support::cfg_str(&spec, "p_flag"),
            "example.com/pkg"
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
            None,
            &[],
            &[],
            V,
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
            None,
            &[],
            &[],
            V,
        );
        assert_eq!(
            crate::plugingo::driver_compile::test_support::cfg_str(&spec, "p_flag"),
            "example.com/pkg_test"
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
            V,
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
            V,
        );
        assert_eq!(
            crate::plugingo::driver_compile::test_support::cfg_str(&spec, "p_flag"),
            "main"
        );
    }

    // ---- build_test_spec ----

    #[test]
    fn test_build_test_spec_driver_is_bash() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            &testmain_lib,
            &[],
            V,
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_build_test_spec_out_has_bin_group() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            &testmain_lib,
            &[],
            V,
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
            &testmain_lib,
            &[],
            V,
        );
        let run = run_str(&spec);
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
            &testmain_lib,
            &[],
            V,
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
    fn test_build_test_lib_spec_carries_go_version() {
        // CGO/GOTOOLCHAIN are pinned inside the go_compile driver now; cache
        // correctness rides on the hashed go_version in the spec.
        let spec = build_test_lib_spec(
            mk_addr("pkg", "build_test_lib"),
            "example.com/pkg",
            "mypkg",
            &test_factors(),
            &[],
            &src_addrs("pkg"),
            &[],
            None,
            &[],
            &[],
            V,
        );
        assert_eq!(spec.driver, "go_compile");
        assert_eq!(
            crate::plugingo::driver_compile::test_support::cfg_str(&spec, "go_version"),
            V
        );
    }

    #[test]
    fn test_build_test_spec_deps_has_testmain() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            &testmain_lib,
            &[],
            V,
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
    fn test_build_test_spec_uses_hermetic_sdk() {
        let testmain_lib = mk_addr("pkg", "build_testmain_lib");
        let spec = build_test_spec(
            mk_addr("pkg", "build_test"),
            &test_factors(),
            &testmain_lib,
            &[],
            V,
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key(crate::plugingo::addr_util::GO_SDK_DEP_GROUP),
            "test link must dep on the hermetic SDK: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        assert!(
            !spec.config.contains_key("tools"),
            "no host go tool should be referenced"
        );
        let run = run_str(&spec);
        assert!(
            run.contains("\"$GO\""),
            "must link with hermetic $GO: {run}"
        );
    }

    // ---- test_spec ----

    fn query_addr(pkg: &str) -> Addr {
        hplugin_query::pluginquery::query_addr(&format!("//{pkg} && label(go_test_data)"), "", &[])
    }

    #[test]
    fn test_test_spec_deps_on_build_test() {
        let build_addr = mk_addr("mypkg", "build_test");
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            build_addr.clone(),
            &query_addr("mypkg"),
            &TestEnv::default(),
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
            &TestEnv::default(),
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
    fn test_test_spec_omits_env_keys_when_test_env_empty() {
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            mk_addr("mypkg", "build_test"),
            &query_addr("mypkg"),
            &TestEnv::default(),
        );
        assert!(!spec.config.contains_key("env"));
        assert!(!spec.config.contains_key("runtime_env"));
        assert!(!spec.config.contains_key("pass_env"));
        assert!(!spec.config.contains_key("runtime_pass_env"));
    }

    #[test]
    fn test_test_spec_plumbs_all_env_knobs() {
        let test_env = TestEnv {
            env: BTreeMap::from([("FOO".to_string(), "1".to_string())]),
            runtime_env: BTreeMap::from([("BAR".to_string(), "2".to_string())]),
            pass_env: vec!["HOME".to_string()],
            runtime_pass_env: vec!["PATH".to_string()],
            pre_run: Vec::new(),
        };
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            mk_addr("mypkg", "build_test"),
            &query_addr("mypkg"),
            &test_env,
        );

        let env = match spec.config.get("env").expect("env present") {
            Value::Map(m) => m.clone(),
            _ => panic!("env must be a map"),
        };
        assert!(matches!(env.get("FOO"), Some(Value::String(s)) if s == "1"));

        let renv = match spec.config.get("runtime_env").expect("runtime_env present") {
            Value::Map(m) => m.clone(),
            _ => panic!("runtime_env must be a map"),
        };
        assert!(matches!(renv.get("BAR"), Some(Value::String(s)) if s == "2"));

        let pass = match spec.config.get("pass_env").expect("pass_env present") {
            Value::List(v) => v.clone(),
            _ => panic!("pass_env must be a list"),
        };
        assert!(matches!(&pass[0], Value::String(s) if s == "HOME"));

        let rpass = match spec
            .config
            .get("runtime_pass_env")
            .expect("runtime_pass_env present")
        {
            Value::List(v) => v.clone(),
            _ => panic!("runtime_pass_env must be a list"),
        };
        assert!(matches!(&rpass[0], Value::String(s) if s == "PATH"));
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
            None,
            &[],
            &[],
            V,
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
            None,
            &[],
            &[],
            V,
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
            V,
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
            &TestEnv::default(),
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
            &TestEnv::default(),
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
            s.contains("label(go_test_data)"),
            "must filter by label: {s}"
        );
        assert!(s.contains("//mypkg"), "must filter by package: {s}");
        assert_eq!(s, qa.format());
    }

    #[test]
    fn test_test_spec_pre_run_switches_to_bash() {
        let test_env = TestEnv {
            pre_run: vec![
                "export FOO=bar".to_string(),
                "mkdir -p ./scratch".to_string(),
            ],
            ..TestEnv::default()
        };
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            mk_addr("mypkg", "build_test"),
            &query_addr("mypkg"),
            &test_env,
        );
        assert_eq!(spec.driver, "bash");
        let lines: Vec<String> = match spec.config.get("run").expect("run present") {
            Value::List(v) => v
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    other => panic!("run line must be string: {other:?}"),
                })
                .collect(),
            other => panic!("run must be a list: {other:?}"),
        };
        // pre_run lines come first, in order, then the test binary invocation.
        assert_eq!(
            lines,
            vec![
                "export FOO=bar".to_string(),
                "mkdir -p ./scratch".to_string(),
                "./test_binary -test.v".to_string(),
            ]
        );
    }

    #[test]
    fn test_test_spec_empty_pre_run_stays_exec() {
        let spec = test_spec(
            mk_addr("mypkg", "test"),
            mk_addr("mypkg", "build_test"),
            &query_addr("mypkg"),
            &TestEnv::default(),
        );
        assert_eq!(spec.driver, "exec");
    }
}
