use crate::plugingo::addr_util::{
    go_build_env, go_goroot_prelude, go_host_runtime_pass_env, go_sdk_dep, go_sdk_read_only_config,
    to_run_value,
};
use crate::plugingo::driver_compile::{CompileParams, build_compile_spec};
use crate::plugingo::factors::Factors;
use crate::plugingo::pkg_analysis::GoPackage;
use crate::plugingo::target_std::archive_filename;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;

/// Build the `download` target that runs `go mod download` for a single
/// `(module, version)` and exposes the module's source tree as artifacts.
///
/// The output group `""` is a `Glob` over `**/*`, capturing every file under
/// the target's pkg dir. Consumers (`build_lib`, embed driver) reference
/// individual files via `TargetAddr` filters; the engine then unpacks only
/// the requested entries into the consumer's sandbox at their pkg-relative
/// paths.
///
/// The stub `go.mod` prevents Go from walking up into the workspace go.mod
/// and downloading every transitive dep; we only want the requested module.
pub fn build_download_spec(
    addr: Addr,
    module: &str,
    version: &str,
    go_version: &str,
) -> TargetSpec {
    let mod_at_ver = format!("{module}@{version}");
    // Multi-line bash. `awk` parses `Dir` out of the JSON `go mod download`
    // emits. The stub go.mod blocks Go from reading the project go.mod.
    //
    // A plain `cp -R`, with no platform branch. This used to pick `cp -RX` on
    // Darwin — BSD `cp` copies xattrs (`com.apple.quarantine` on Go's mod cache
    // files), which EPERMs on a FUSE overlay that rejects xattr writes, and `-X`
    // suppresses that.
    //
    // `-X` is BSD-only — GNU `cp` rejects it outright (`invalid option -- 'X'`)
    // — and `uname` no longer tells you which `cp` you have: it says Darwin for
    // a Mac inside a devenv, where `PATH` leads with nixpkgs coreutils and the
    // build broke. `PATH` is the host's under `gotool = "host"` and entirely the
    // runner's under a `runner:`, so nothing here can know the dialect. Naming
    // `/bin/cp` would answer the dialect and lose the sandbox: it is a host path
    // that a container image or a NixOS box need not have at all.
    //
    // So the flag goes. Its only exposure is macOS under `fuse.enabled`, where
    // the EPERM may return; elsewhere the copied xattrs are inert metadata on
    // scratch files, and Linux has none to copy. FUSE is already an untested
    // gap for runners (`docs/EXEC_RUNNERS.md`). Deliberate call, taken to keep
    // the copy hermetic and dialect-neutral.
    //
    // GOROOT/`go` come from the staged hermetic SDK. We use the GOROOT-only
    // prelude (no sandbox GOCACHE) because the `**/*` output glob would
    // otherwise capture the cache dir; `go mod download` doesn't need GOCACHE.
    let mut run = go_goroot_prelude(go_version);
    run.extend([
        "printf 'module heph_ignore\\n' > go.mod".to_string(),
        format!("\"$GO\" mod download -modcacherw -json '{mod_at_ver}' > mod.json"),
        "rm go.mod".to_string(),
        "MOD_DIR=$(awk -F'\"' '/\"Dir\": / { print $4 }' mod.json)".to_string(),
        // One compound statement — keep its lines in a single entry.
        "if [ -z \"$MOD_DIR\" ]; then\n\
           echo 'go mod download produced no Dir' >&2\n\
           cat mod.json >&2\n\
           exit 1\n\
         fi"
        .to_string(),
        "cp -R \"$MOD_DIR/.\" .".to_string(),
        "rm mod.json".to_string(),
        "chmod -R u+w .".to_string(),
    ]);

    let mut deps: HashMap<String, Value> = HashMap::new();
    if let Some((sdk_group, sdk_val)) = go_sdk_dep(go_version) {
        deps.insert(sdk_group, sdk_val);
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert("deps".to_string(), Value::Map(deps));
    if let Some((ro_k, ro_v)) = go_sdk_read_only_config(go_version) {
        config.insert(ro_k, ro_v);
    }
    // The shared module cache. Env-var-only — no mount — which is what makes it
    // possible here at all: this target collects `out = "**/*"` from its package,
    // and an in-tree mount would be swept straight into the artifact. `go mod
    // download` reads `GOMODCACHE` from the environment and does not care where
    // the directory is.
    config.insert(
        "scratch".to_string(),
        Value::List(vec![Value::String(
            crate::plugingo::gocache::modcache_addr().format(),
        )]),
    );
    // Glob form (contains `*`) — pluginexec packs every matching file under
    // the target's pkg into the artifact, preserving relative paths.
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            String::new(),
            Value::List(vec![Value::String("**/*".to_string())]),
        )])),
    );
    // CGO/toolchain pins live in `env` (hashed; pluginexec/mod.rs:70 excludes
    // runtime_env from the def hash).
    config.insert("env".to_string(), go_build_env());
    // The module cache is heph's now, not the host's — a scratch declared at
    // `//@heph/go/gocache:modcache` and announced through `GOMODCACHE` (see the
    // `scratch` config below). So `GOMODCACHE` is deliberately *absent* from the
    // passthrough list: leaving it would let a host value reach the sandbox and
    // collide with the one the scratch sets, which pluginexec rejects rather
    // than silently picking a winner.
    //
    // The rest still come from the host. `GOPROXY` and friends are network
    // configuration, not cache placement — how to reach a registry is the user's
    // to decide, and heph owning where the bytes land does not change that. For a
    // host/target toolchain, `go` itself is non-hermetic and must be resolvable
    // from the sandbox — merge in `go_host_runtime_pass_env` (PATH, HOME, …)
    // rather than clobbering it with a second `runtime_pass_env` insert.
    let mut pass_env: Vec<String> = [
        "HOME",
        "GOPATH",
        "GOCACHE",
        "GOPROXY",
        "GONOSUMDB",
        "GOSUMDB",
        "GOFLAGS",
        "GOPRIVATE",
        "GONOSUMCHECK",
        "GOINSECURE",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    for name in go_host_runtime_pass_env(go_version) {
        if !pass_env.contains(&name) {
            pass_env.push(name);
        }
    }
    config.insert(
        "runtime_pass_env".to_string(),
        Value::List(pass_env.into_iter().map(Value::String).collect()),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        ..Default::default()
    }
}

/// Build the thirdparty library compile (`go_compile`). Same driver as
/// first-party; the asm/symabis/pack pipeline is driven by `pkg.s_files`.
///
/// `golist_addr` is `Some` iff the package embeds (module-shipped embed files);
/// thirdparty has no `go_embed_src` lane.
#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required, no natural grouping"
)]
pub fn build_lib_spec(
    addr: Addr,
    pkg: &GoPackage,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    src_addrs: &[String],
    s_file_addrs: &[String],
    h_file_addrs: &[String],
    extra_h_file_addrs: &[String],
    golist_addr: Option<&Addr>,
    embed_file_addrs: &[String],
    go_version: &str,
) -> TargetSpec {
    let import_path = &pkg.import_path;
    let package_name = pkg.name.as_deref().unwrap_or("");
    let out_file = archive_filename(import_path);
    let p_flag = if package_name == "main" {
        "main".to_string()
    } else {
        import_path.clone()
    };

    // .h headers next to the .s files plus sibling-package headers the asm
    // `#include`s across the module — staged together in the `hdr` group.
    let hdrs: Vec<String> = h_file_addrs
        .iter()
        .chain(extra_h_file_addrs.iter())
        .cloned()
        .collect();

    build_compile_spec(CompileParams {
        addr,
        p_flag,
        out_file,
        factors,
        go_version,
        transitive_libs,
        src_addrs,
        s_files: &pkg.s_files,
        s_file_addrs,
        hdr_addrs: &hdrs,
        golist_addr,
        embed_variant: if golist_addr.is_some() { "embed" } else { "" },
        embed_file_addrs,
        embed_src_addrs: &[],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugingo::pkg_analysis::{GoModule, GoPackage};
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

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
            ..Default::default()
        }
    }

    fn test_addr() -> Addr {
        Addr::new(
            PkgBuf::from("@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2"),
            "build_lib".to_string(),
            Default::default(),
        )
    }

    fn download_addr() -> Addr {
        Addr::new(
            PkgBuf::from("@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2"),
            "download".to_string(),
            Default::default(),
        )
    }

    fn test_pkg(go_files: Vec<String>) -> GoPackage {
        test_pkg_with_asm(go_files, vec![])
    }

    fn test_pkg_with_asm(go_files: Vec<String>, s_files: Vec<String>) -> GoPackage {
        GoPackage {
            import_path: "github.com/go-logr/logr".to_string(),
            dir: Some("/home/user/go/pkg/mod/github.com/go-logr/logr@v1.4.2".to_string()),
            name: Some("logr".to_string()),
            go_files,
            s_files,
            h_files: vec![],
            test_go_files: vec![],
            xtest_go_files: vec![],
            embed_patterns: vec![],
            embed_files: vec![],
            test_embed_patterns: vec![],
            test_embed_files: vec![],
            xtest_embed_patterns: vec![],
            xtest_embed_files: vec![],
            imports: vec![],
            test_imports: vec![],
            xtest_imports: vec![],
            standard: false,
            module: Some(GoModule {
                path: "github.com/go-logr/logr".to_string(),
                version: Some("v1.4.2".to_string()),
                dir: Some("/home/user/go/pkg/mod/github.com/go-logr/logr@v1.4.2".to_string()),
            }),
            match_: vec![],
            incomplete: false,
            error: None,
        }
    }

    fn filter_src_addr(file: &str) -> String {
        format!(
            "//@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2:download[@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2/{file}]"
        )
    }

    // ---- download spec ----

    /// The module copy must run under whatever `cp` the environment provides.
    ///
    /// It used to be `/bin/cp -RX` on Darwin. `-X` (suppress xattr copy) is a
    /// BSD flag that GNU `cp` rejects outright, and `uname` cannot tell the two
    /// apart any more: a Mac inside a devenv reports Darwin while its `PATH`
    /// leads with nixpkgs coreutils. `/bin/cp` answered the dialect by reaching
    /// outside the sandbox for a host binary a container or a NixOS box need not
    /// have.
    ///
    /// So neither may come back. Dropping `-X` is a deliberate trade: its only
    /// exposure is macOS under `fuse.enabled`, where the EPERM it avoided may
    /// return.
    #[test]
    fn test_download_spec_copies_module_with_a_portable_cp() {
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        let run = run_str(&spec);
        assert!(
            run.contains("cp -R \"$MOD_DIR/.\" ."),
            "the module copy must be a plain, PATH-resolved `cp -R`: {run}"
        );
        assert!(!run.contains("/bin/cp"), "must not name a host path: {run}");
        assert!(
            !run.contains("uname"),
            "the OS does not determine the `cp` dialect once a runner supplies \
             the environment: {run}"
        );
    }

    #[test]
    fn test_download_spec_driver() {
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_download_spec_run_contains_go_mod_download() {
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        let run = run_str(&spec);
        assert!(
            run.contains("go mod download"),
            "missing go mod download: {run}"
        );
        assert!(
            run.contains("github.com/go-logr/logr@v1.4.2"),
            "missing mod@ver literal: {run}"
        );
        assert!(
            run.contains("module heph_ignore"),
            "missing stub go.mod: {run}"
        );
        assert!(run.contains("$GO"), "must call hermetic $GO: {run}");
    }

    #[test]
    fn test_download_spec_out_is_glob() {
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        let default = match out.get("").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            matches!(&default[0], Value::String(s) if s == "**/*"),
            "default output group must be Glob **/*: {:?}",
            default
        );
    }

    #[test]
    fn test_download_spec_uses_hermetic_sdk() {
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        assert!(
            !spec.config.contains_key("tools"),
            "download must not reference a host go tool"
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key(crate::plugingo::addr_util::GO_SDK_DEP_GROUP),
            "download must dep on the hermetic SDK so `go` is staged: {deps:?}"
        );
        let run = run_str(&spec);
        assert!(
            run.contains(&format!(
                "$WORKSPACE_ROOT/{}",
                crate::plugingo::toolchain::staged_goroot(V)
            )),
            "download must point GOROOT at the staged SDK: {run}"
        );
    }

    fn pass_env(spec: &TargetSpec) -> Vec<String> {
        match spec.config.get("runtime_pass_env").unwrap() {
            Value::List(v) => v
                .iter()
                .map(|e| match e {
                    Value::String(s) => s.clone(),
                    _ => panic!("pass-env entry not a string"),
                })
                .collect(),
            _ => panic!("runtime_pass_env not a list"),
        }
    }

    #[test]
    fn test_download_host_toolchain_passes_path_and_module_env() {
        // Host `go` is resolved from the sandbox PATH, so PATH must be inherited
        // — the earlier double `runtime_pass_env` insert clobbered it, breaking
        // `gotool: host` with `go: command not found`.
        let spec = build_download_spec(
            download_addr(),
            "github.com/go-logr/logr",
            "v1.4.2",
            crate::plugingo::toolchain::HOST,
        );
        let env = pass_env(&spec);
        assert!(
            env.contains(&"PATH".to_string()),
            "host must pass PATH: {env:?}"
        );
        // Module/proxy knobs stay inherited too (merged, not lost).
        for v in ["GOMODCACHE", "GOPROXY", "GOCACHE"] {
            assert!(env.contains(&v.to_string()), "must keep {v}: {env:?}");
        }
    }

    #[test]
    fn test_download_hermetic_toolchain_does_not_pass_path() {
        // Hermetic `go` is staged, never resolved from PATH.
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        let env = pass_env(&spec);
        assert!(
            !env.contains(&"PATH".to_string()),
            "hermetic must not leak host PATH: {env:?}"
        );
        // The module cache is heph's, declared as a scratch and announced through
        // `GOMODCACHE`. Passing the host's through as well would collide with it,
        // which pluginexec rejects rather than silently picking a winner.
        assert!(
            !env.contains(&"GOMODCACHE".to_string()),
            "GOMODCACHE is set by the scratch, not inherited: {env:?}"
        );
        // Network configuration is still the user's: how to reach a registry is a
        // different question from where the bytes land.
        assert!(
            env.contains(&"GOPROXY".to_string()),
            "must keep the network env: {env:?}"
        );
    }

    /// The download target references the shared module cache, and does so
    /// without a mount — its `out = "**/*"` would otherwise sweep the cache into
    /// the artifact.
    #[test]
    fn test_download_references_the_shared_modcache() {
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        let scratch = match spec.config.get("scratch").expect("scratch config") {
            Value::List(v) => v.clone(),
            other => panic!("expected a list, got {other:?}"),
        };
        assert_eq!(
            scratch,
            vec![Value::String(
                crate::plugingo::gocache::modcache_addr().format()
            )]
        );
    }

    // ---- build_lib_spec ----

    #[test]
    fn test_download_spec_env_pins_cgo_disabled() {
        let spec = build_download_spec(download_addr(), "github.com/go-logr/logr", "v1.4.2", V);
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"),
            "download env must pin CGO_ENABLED=0 in the hashed map: {:?}",
            env.get("CGO_ENABLED")
        );
    }

    use crate::plugingo::driver_compile::test_support::{cfg_list, cfg_str, deps_map};

    fn golist_addr() -> Addr {
        Addr::new(
            PkgBuf::from("@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2"),
            "_golist".to_string(),
            Default::default(),
        )
    }

    fn lib_spec(
        pkg: &GoPackage,
        transitive: &[(String, Addr)],
        golist: Option<&Addr>,
        embed_files: &[String],
    ) -> TargetSpec {
        build_lib_spec(
            test_addr(),
            pkg,
            &test_factors(),
            transitive,
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            &[],
            golist,
            embed_files,
            V,
        )
    }

    #[test]
    fn test_build_lib_driver_is_go_compile() {
        assert_eq!(
            lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[]).driver,
            "go_compile"
        );
    }

    #[test]
    fn test_build_lib_has_go_build_label() {
        assert!(
            lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[])
                .labels
                .contains(&"go-build".to_string())
        );
    }

    #[test]
    fn test_build_lib_out_has_a_group() {
        let s = lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[]);
        assert!(matches!(s.config.get("out").unwrap(), Value::Map(m) if m.contains_key("a")));
    }

    #[test]
    fn test_build_lib_src_in_default_group() {
        let s = lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[]);
        assert_eq!(
            deps_map(&s).get("").unwrap(),
            &vec![filter_src_addr("logr.go")]
        );
    }

    #[test]
    fn test_build_lib_uses_hermetic_sdk() {
        let s = lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[]);
        assert!(deps_map(&s).contains_key(crate::plugingo::addr_util::GO_SDK_DEP_GROUP));
    }

    #[test]
    fn test_build_lib_main_package_uses_p_main() {
        let mut pkg = test_pkg(vec!["logr.go".into()]);
        pkg.name = Some("main".to_string());
        assert_eq!(cfg_str(&lib_spec(&pkg, &[], None, &[]), "p_flag"), "main");
    }

    #[test]
    fn test_build_lib_non_main_uses_import_path() {
        assert_eq!(
            cfg_str(
                &lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[]),
                "p_flag"
            ),
            "github.com/go-logr/logr"
        );
    }

    #[test]
    fn test_build_lib_transitive_dep_in_import_paths() {
        let dep = (
            "fmt".to_string(),
            Addr::new(
                PkgBuf::from("@heph/go/std/fmt"),
                "build_lib".to_string(),
                Default::default(),
            ),
        );
        let s = lib_spec(
            &test_pkg(vec!["logr.go".into()]),
            std::slice::from_ref(&dep),
            None,
            &[],
        );
        assert!(cfg_list(&s, "import_paths").contains(&"fmt".to_string()));
        assert!(deps_map(&s).contains_key("lib_fmt"));
    }

    #[test]
    fn test_build_lib_s_files_drive_asm_group() {
        let pkg = test_pkg_with_asm(vec!["logr.go".into()], vec!["asm_amd64.s".into()]);
        let s = build_lib_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[filter_src_addr("asm_amd64.s")],
            &[],
            &[],
            None,
            &[],
            V,
        );
        assert_eq!(cfg_list(&s, "s_files"), vec!["asm_amd64.s".to_string()]);
        assert!(deps_map(&s).contains_key("asm"));
    }

    #[test]
    fn test_build_lib_no_s_files_no_asm_group() {
        let s = lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[]);
        assert!(cfg_list(&s, "s_files").is_empty());
        assert!(!deps_map(&s).contains_key("asm"));
    }

    #[test]
    fn test_build_lib_extra_h_files_join_hdr_group() {
        let pkg = test_pkg_with_asm(vec!["logr.go".into()], vec!["asm_amd64.s".into()]);
        let s = build_lib_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[filter_src_addr("asm_amd64.s")],
            &[filter_src_addr("abi.h")],
            &[filter_src_addr("sibling.h")],
            None,
            &[],
            V,
        );
        let hdr = deps_map(&s).get("hdr").cloned().unwrap_or_default();
        assert!(
            hdr.iter().any(|h| h.contains("abi.h")),
            "hdr group missing abi.h: {hdr:?}"
        );
        assert!(
            hdr.iter().any(|h| h.contains("sibling.h")),
            "hdr group missing sibling.h: {hdr:?}"
        );
    }

    #[test]
    fn test_build_lib_embed_sets_variant_and_golist_dep() {
        let mut pkg = test_pkg(vec!["logr.go".into()]);
        pkg.embed_patterns = vec!["t.txt".to_string()];
        let g = golist_addr();
        let s = lib_spec(&pkg, &[], Some(&g), &[filter_src_addr("t.txt")]);
        assert_eq!(cfg_list(&s, "embed_variant"), vec!["embed".to_string()]);
        assert!(deps_map(&s).contains_key("golist"));
        assert!(deps_map(&s).contains_key("embed_files"));
    }

    #[test]
    fn test_build_lib_no_embed_variant_without_golist() {
        let s = lib_spec(&test_pkg(vec!["logr.go".into()]), &[], None, &[]);
        assert!(cfg_list(&s, "embed_variant").is_empty());
    }
}
