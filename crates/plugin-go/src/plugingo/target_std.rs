use crate::plugingo::addr_util::{
    go_host_pass_env_config, go_run_prelude, go_sdk_dep, go_sdk_read_only_config, to_run_value,
};
use crate::plugingo::cc_toolchain;
use crate::plugingo::factors::{Factors, VariantRef};
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;

/// Package the std-library `install` target lives in.
pub const STD_PKG: &str = "@heph/go/std";

/// Derive a unique archive filename from an import path so that multiple archives
/// can coexist in the same sandbox directory without overwriting each other.
/// e.g. "fmt" → "fmt.a", "internal/chacha8rand" → "internal_chacha8rand.a"
pub fn archive_filename(import_path: &str) -> String {
    let sanitized: String = import_path
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{}.a", sanitized)
}

/// Address of the `@heph/go/std:install` target for the given variant.
pub fn install_addr(vref: &VariantRef) -> Addr {
    Addr::new(PkgBuf::from(STD_PKG), "install".to_string(), vref.to_args())
}

/// Sandbox-relative dir the std archives land in after `go install std`, e.g.
/// `goroot/pkg/linux_amd64`. The install target's package (`@heph/go/std`) is
/// prepended by the engine, so consumers read from
/// `$WORKSPACE_ROOT/@heph/go/std/goroot/pkg/<goos>_<goarch>`.
///
/// A race build installs alongside under `<goos>_<goarch>_race` — the
/// `-installsuffix race` layout `go install -race` writes and `go tool link`
/// expects. The two sets are genuinely different archives (every std package is
/// instrumented, and `//go:build race` files change the sources), so they must
/// not share a directory.
fn pkg_subdir(factors: &Factors) -> String {
    let suffix = if factors.race { "_race" } else { "" };
    format!("goroot/pkg/{}_{}{}", factors.goos, factors.goarch, suffix)
}

/// Build the `@heph/go/std:install` target: compile the *entire* standard
/// library from source using the hermetic SDK, mirroring the `v1` plugin's
/// `stdInstall`. A fresh Go SDK ships no precompiled `pkg/*.a`, so we copy
/// `$GOROOT` to a writable tree and `go install std` into it (with
/// `installgoroot=all`), then dump `go list -json std` for downstream metadata.
/// Reads nothing from the host — `$GOROOT` is the staged toolchain.
pub fn install_spec(addr: Addr, factors: &Factors, go_version: &str, cc_addr: &str) -> TargetSpec {
    let subdir = pkg_subdir(factors);

    let mut run = go_run_prelude(go_version);
    // A race build that needs cgo compiles C here (`runtime/cgo`, and the cgo
    // half of `runtime/race`), so point `go` at the staged compiler. No-op for
    // every other build — see `cc_toolchain`.
    run.extend(cc_toolchain::cc_prelude(&factors.goos, factors.race));
    run.extend([
        // Copy the staged GOROOT into a writable tree so `go install` can
        // populate pkg/. `-L` dereferences symlinks: staged dep files may be
        // symlinks into the read-only artifact cache, and a plain copy would
        // leave LGOROOT pointing back at it (so `chmod -R u+w` would try to
        // mutate the cache). BSD/macOS `cp` also copies xattrs and trips EPERM
        // on the FUSE overlay; `cp -X` suppresses that (GNU `cp -X` differs, so
        // branch on uname). Use the absolute system `cp` (`/bin/cp`) so the flag
        // set matches the platform regardless of `$PATH` — under the host
        // toolchain (`gotool = "host"`) the sandbox inherits the host `PATH`,
        // which may put a GNU `cp` (no `-X`) ahead of the BSD system one.
        "if [ \"$(uname)\" = Darwin ]; then CP=\"/bin/cp -RLX\"; else CP=\"/bin/cp -RL\"; fi"
            .to_string(),
        "LGOROOT=\"$PWD/goroot\"".to_string(),
        "rm -rf \"$LGOROOT\"".to_string(),
        "$CP \"$GOROOT\" \"$LGOROOT\"".to_string(),
        "export GOROOT=\"$LGOROOT\"".to_string(),
        "chmod -R u+w \"$GOROOT\"".to_string(),
        // A throwaway module keeps `go` from reading the workspace go.mod.
        "echo 'module heph_std' > go.mod".to_string(),
        format!(
            "\"$GOROOT/bin/go\" install --trimpath{} std",
            if factors.race { " -race" } else { "" }
        ),
        format!(
            "\"$GOROOT/bin/go\" list{} -json std > \"{subdir}/list.json\"",
            if factors.race { " -race" } else { "" }
        ),
    ]);

    let mut deps: HashMap<String, Value> = HashMap::new();
    if let Some((sdk_group, sdk_val)) = go_sdk_dep(go_version) {
        deps.insert(sdk_group, sdk_val);
    }
    if let Some((cc_group, cc_val)) = cc_toolchain::cc_dep(cc_addr, &factors.goos, factors.race) {
        deps.insert(cc_group, cc_val);
    }

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert("deps".to_string(), Value::Map(deps));
    if let Some((ro_k, ro_v)) = go_sdk_read_only_config(go_version) {
        config.insert(ro_k, ro_v);
    }
    if let Some((pe_k, pe_v)) = go_host_pass_env_config(go_version) {
        config.insert(pe_k, pe_v);
    }
    // A staged C compiler still needs `PATH` to reach its own subprograms (`as`,
    // `ld`); without it cgo fails with `cannot execute 'as'`. Merged rather than
    // set so a non-hermetic toolchain's passthrough above survives. No-op unless
    // this build needs a compiler.
    cc_toolchain::merge_runtime_pass_env(&mut config, &factors.goos, factors.race);
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([
            (
                "pkg".to_string(),
                Value::List(vec![Value::String(format!("{subdir}/"))]),
            ),
            (
                "list".to_string(),
                Value::List(vec![Value::String(format!("{subdir}/list.json"))]),
            ),
        ])),
    );
    // Hashed env pins the target platform + cgo so std archives don't bleed
    // across factor variants. GODEBUG forces `go install std` to write pkg/.
    config.insert(
        "env".to_string(),
        Value::Map(HashMap::from([
            ("GOOS".to_string(), Value::String(factors.goos.clone())),
            ("GOARCH".to_string(), Value::String(factors.goarch.clone())),
            (
                "CGO_ENABLED".to_string(),
                Value::String(factors.cgo_enabled_value().to_string()),
            ),
            (
                "GOTOOLCHAIN".to_string(),
                Value::String("local".to_string()),
            ),
            ("GOWORK".to_string(), Value::String("off".to_string())),
            (
                "GODEBUG".to_string(),
                Value::String("installgoroot=all".to_string()),
            ),
        ])),
    );

    TargetSpec {
        addr,
        driver: "bash".to_string(),
        config,
        labels: vec!["go-build".to_string(), "go-std-install".to_string()],
        ..Default::default()
    }
}

/// Build the `@heph/go/std/<import>:build_lib` target: extract a single
/// precompiled archive from the `install` output. Mirrors `v1`'s `stdLibBuild` —
/// pure `mv`, no Go invocation, so it carries no SDK dep.
pub fn build_spec(
    addr: Addr,
    import_path: &str,
    factors: &Factors,
    vref: &VariantRef,
) -> TargetSpec {
    let out_file = archive_filename(import_path);
    let subdir = pkg_subdir(factors);

    let run = vec![
        // The install target's `pkg` output is staged at this stable path.
        format!("mv \"$WORKSPACE_ROOT/{STD_PKG}/{subdir}/{import_path}.a\" \"{out_file}\""),
    ];

    let mut deps: HashMap<String, Value> = HashMap::new();
    deps.insert(
        "install".to_string(),
        Value::List(vec![Value::String(format!(
            "{}|pkg",
            install_addr(vref).format()
        ))]),
    );

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert("deps".to_string(), Value::Map(deps));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "a".to_string(),
            Value::List(vec![Value::String(out_file)]),
        )])),
    );

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec!["go-build".to_string()],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn build_lib_addr() -> Addr {
        Addr::new(
            PkgBuf::from("@heph/go/std/fmt"),
            "build_lib".to_string(),
            Default::default(),
        )
    }

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
            ..Default::default()
        }
    }

    fn test_vref() -> VariantRef {
        VariantRef::new("dev", "")
    }

    #[test]
    fn test_install_builds_std_from_source() {
        let spec = install_spec(
            install_addr(&test_vref()),
            &test_factors(),
            V,
            "//@heph/bin:cc",
        );
        let run = run_str(&spec);
        assert!(
            run.contains("install --trimpath std"),
            "install must compile std from source: {run}"
        );
        assert!(run.contains("list -json std"), "install must dump std list");
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_install_reads_no_host_go() {
        let spec = install_spec(
            install_addr(&test_vref()),
            &test_factors(),
            V,
            "//@heph/bin:cc",
        );
        let run = run_str(&spec);
        // GOROOT must come from the staged hermetic SDK, never the host.
        assert!(
            run.contains(&format!(
                "$WORKSPACE_ROOT/{}",
                crate::plugingo::toolchain::staged_goroot(V)
            )),
            "GOROOT must be the staged toolchain: {run}"
        );
        // The install must depend on the hermetic SDK.
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("deps must be a map"),
        };
        assert!(
            deps.contains_key(crate::plugingo::addr_util::GO_SDK_DEP_GROUP),
            "install must dep on the hermetic SDK"
        );
    }

    #[test]
    fn test_install_out_has_pkg_dir_and_list() {
        let spec = install_spec(
            install_addr(&test_vref()),
            &test_factors(),
            V,
            "//@heph/bin:cc",
        );
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("out must be map"),
        };
        assert!(matches!(
            out.get("pkg").unwrap(),
            Value::List(v) if matches!(&v[0], Value::String(s) if s == "goroot/pkg/linux_amd64/")
        ));
        assert!(out.contains_key("list"));
    }

    #[test]
    fn test_install_env_sets_installgoroot_and_target_platform() {
        let factors = Factors {
            goos: "darwin".into(),
            goarch: "arm64".into(),
            build_tags: vec![],
            ..Default::default()
        };
        let spec = install_spec(install_addr(&test_vref()), &factors, V, "//@heph/bin:cc");
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("env must be map"),
        };
        assert!(matches!(env.get("GODEBUG"), Some(Value::String(s)) if s == "installgoroot=all"));
        assert!(matches!(env.get("GOOS"), Some(Value::String(s)) if s == "darwin"));
        assert!(matches!(env.get("GOARCH"), Some(Value::String(s)) if s == "arm64"));
    }

    #[test]
    fn test_build_lib_extracts_from_install() {
        let spec = build_spec(build_lib_addr(), "fmt", &test_factors(), &test_vref());
        let run = run_str(&spec);
        assert!(
            run.contains("@heph/go/std/goroot/pkg/linux_amd64/fmt.a"),
            "build_lib must mv the archive from the install output: {run}"
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_build_lib_deps_on_install() {
        let spec = build_spec(build_lib_addr(), "fmt", &test_factors(), &test_vref());
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("deps must be map"),
        };
        let install = match deps.get("install").unwrap() {
            Value::List(v) => v,
            _ => panic!("install dep must be list"),
        };
        assert!(
            matches!(&install[0], Value::String(s) if s.contains("@heph/go/std:install") && s.ends_with("|pkg")),
            "build_lib must dep on the install pkg output: {install:?}"
        );
    }

    #[test]
    fn test_build_lib_carries_no_sdk_dep() {
        // build_lib is a pure mv; pulling the whole SDK in would be wasteful.
        let spec = build_spec(build_lib_addr(), "fmt", &test_factors(), &test_vref());
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("deps must be map"),
        };
        assert!(!deps.contains_key(crate::plugingo::addr_util::GO_SDK_DEP_GROUP));
    }

    #[test]
    fn test_build_lib_out_group() {
        let spec = build_spec(build_lib_addr(), "fmt", &test_factors(), &test_vref());
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, Value::Map(m) if m.contains_key("a")));
    }

    #[test]
    fn test_archive_filename_sanitizes() {
        assert_eq!(archive_filename("fmt"), "fmt.a");
        assert_eq!(
            archive_filename("internal/chacha8rand"),
            "internal_chacha8rand.a"
        );
    }

    // ---- race ----

    fn race_factors(goos: &str) -> Factors {
        Factors {
            goos: goos.into(),
            goarch: "arm64".into(),
            race: true,
            ..Default::default()
        }
    }

    const CC: &str = "//@heph/bin:cc";

    /// A race build installs std with `-race`, into the `_race` directory
    /// `go install -race` writes and the linker expects. The two archive sets are
    /// genuinely different, so they must not share a directory.
    #[test]
    fn race_install_uses_race_flag_and_its_own_pkg_dir() {
        let spec = install_spec(install_addr(&test_vref()), &race_factors("linux"), V, CC);
        let run = run_str(&spec);
        assert!(
            run.contains("install --trimpath -race std"),
            "std must be compiled with -race: {run}"
        );
        assert!(
            run.contains("goroot/pkg/linux_arm64_race"),
            "race archives belong in their own pkg dir: {run}"
        );
        // `go list` has to agree with the install, or it reports the non-race
        // file set and import graph for archives that were built racy.
        assert!(run.contains("list -race -json std"), "{run}");
    }

    #[test]
    fn non_race_install_is_unchanged() {
        let spec = install_spec(install_addr(&test_vref()), &test_factors(), V, CC);
        let run = run_str(&spec);
        assert!(run.contains("install --trimpath std"), "{run}");
        assert!(
            !run.contains("-race"),
            "no race flag on an ordinary build: {run}"
        );
        assert!(!run.contains("_race"), "{run}");
    }

    /// `build_lib` extracts from whichever pkg dir its factors name, so a race
    /// consumer reads the race archive rather than silently getting the ordinary
    /// one.
    #[test]
    fn race_build_lib_reads_the_race_archive() {
        let vref = test_vref().with_race(true);
        let spec = build_spec(
            build_lib_addr(),
            "runtime/race",
            &race_factors("linux"),
            &vref,
        );
        let run = run_str(&spec);
        assert!(
            run.contains("goroot/pkg/linux_arm64_race/runtime/race.a"),
            "{run}"
        );
        // …and it deps on the race flavour of the install, not the ordinary one.
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m.clone(),
            other => panic!("expected deps map: {other:?}"),
        };
        let install = match deps.get("install").unwrap() {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.clone(),
                other => panic!("expected string: {other:?}"),
            },
            other => panic!("expected list: {other:?}"),
        };
        assert!(install.contains("race=1"), "{install}");
    }

    /// linux race needs cgo, so the compiler is staged and `CC` points at it.
    #[test]
    fn linux_race_install_stages_a_c_compiler() {
        let spec = install_spec(install_addr(&test_vref()), &race_factors("linux"), V, CC);
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m.clone(),
            other => panic!("expected deps map: {other:?}"),
        };
        assert!(
            deps.contains_key(crate::plugingo::cc_toolchain::CC_DEP_GROUP),
            "linux race must dep on a C compiler: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        assert!(run_str(&spec).contains("export CC"), "{}", run_str(&spec));
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m.clone(),
            other => panic!("expected env map: {other:?}"),
        };
        assert!(matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "1"));
        // The compiler finds `as`/`ld` through PATH; without it cgo dies with
        // `cannot execute 'as'`.
        let pass = match spec.config.get("runtime_pass_env") {
            Some(Value::List(v)) => v.clone(),
            other => panic!("expected runtime_pass_env list: {other:?}"),
        };
        assert!(pass.contains(&Value::String("PATH".to_string())));
    }

    /// darwin race stays hermetic: `runtime/race` is pure Go plus a syso there,
    /// so no compiler dep, no cgo, no PATH passthrough.
    #[test]
    fn darwin_race_install_needs_no_c_compiler() {
        let spec = install_spec(install_addr(&test_vref()), &race_factors("darwin"), V, CC);
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m.clone(),
            other => panic!("expected deps map: {other:?}"),
        };
        assert!(!deps.contains_key(crate::plugingo::cc_toolchain::CC_DEP_GROUP));
        assert!(!run_str(&spec).contains("export CC"));
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m.clone(),
            other => panic!("expected env map: {other:?}"),
        };
        assert!(matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"));
        assert!(!spec.config.contains_key("runtime_pass_env"));
    }
}
