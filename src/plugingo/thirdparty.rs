use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::loosespecparser::TargetSpecValue;
use crate::plugingo::addr_util::{import_path_to_dep_group, write_importcfg_script};
use crate::plugingo::factors::Factors;
use crate::plugingo::pkg_analysis::GoPackage;
use crate::plugingo::target_std::archive_filename;
use std::collections::{BTreeMap, HashMap};

pub fn build_spec(
    addr: Addr,
    pkg: &GoPackage,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    go_bin_addr: &str,
    goroot: &str,
) -> TargetSpec {
    let import_path = &pkg.import_path;
    let package_name = pkg.name.as_deref().unwrap_or("");
    let pkg_dir = pkg.dir.as_deref().unwrap_or("");

    let out_file = archive_filename(import_path);
    // Go requires main packages to be compiled with -p "main".
    let p_flag = if package_name == "main" {
        "main"
    } else {
        import_path.as_str()
    };

    let run = generate_compile_script(
        p_flag,
        transitive_libs,
        &out_file,
        pkg_dir,
        &pkg.go_files,
        &pkg.s_files,
        &factors.goos,
        &factors.goarch,
    );

    let mut deps: BTreeMap<String, TargetSpecValue> = transitive_libs
        .iter()
        .map(|(dep_import_path, dep_addr)| {
            let group = import_path_to_dep_group(dep_import_path);
            (
                group,
                TargetSpecValue::List(vec![TargetSpecValue::String(dep_addr.format())]),
            )
        })
        .collect();
    deps.insert(
        "go_bin".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(go_bin_addr.to_string())]),
    );

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

fn generate_compile_script(
    p_flag: &str,
    transitive_libs: &[(String, Addr)],
    out_file: &str,
    pkg_dir: &str,
    go_files: &[String],
    s_files: &[String],
    goos: &str,
    goarch: &str,
) -> String {
    let mut script = write_importcfg_script(transitive_libs, None);

    // Source files live in GOMODCACHE (read-only, content-stable) — use absolute paths.
    let src_args = go_files
        .iter()
        .map(|f| format!("\"{pkg_dir}/{f}\""))
        .collect::<Vec<_>>()
        .join(" ");

    let has_asm = !s_files.is_empty();
    let asmhdr_flag = if has_asm { " -asmhdr \"go_asm.h\"" } else { "" };

    script.push_str(&format!(
        "\"$SRC_GO_BIN\" tool compile -p \"{p_flag}\" -trimpath -pack -importcfg \"$importcfg\"{asmhdr_flag} -o \"{out_file}\" {src_args}\n",
    ));

    if has_asm {
        for s_file in s_files {
            let obj_file = format!("{}.o", s_file.trim_end_matches(".s"));
            script.push_str(&format!(
                "\"$SRC_GO_BIN\" tool asm -p \"{p_flag}\" -trimpath -I \"{pkg_dir}\" -I \"$GOROOT/pkg/include\" -D GOOS_{goos} -D GOARCH_{goarch} -o \"{obj_file}\" \"{pkg_dir}/{s_file}\"\n"
            ));
        }
        let obj_args = s_files
            .iter()
            .map(|f| format!("\"{}\"", format!("{}.o", f.trim_end_matches(".s"))))
            .collect::<Vec<_>>()
            .join(" ");
        script.push_str(&format!(
            "\"$SRC_GO_BIN\" tool pack r \"{out_file}\" {obj_args}\n"
        ));
    }

    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;
    use crate::plugingo::pkg_analysis::{GoModule, GoPackage};

    fn test_factors() -> Factors {
        Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
        }
    }

    fn test_addr() -> Addr {
        Addr {
            package: PkgBuf::from("@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2"),
            name: "build_lib".to_string(),
            args: Default::default(),
        }
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
            test_go_files: vec![],
            xtest_go_files: vec![],
            embed_patterns: vec![],
            embed_files: vec![],
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

    #[test]
    fn test_driver_is_bash() {
        let spec = build_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        assert_eq!(spec.driver, "bash");
    }

    #[test]
    fn test_run_uses_absolute_src_paths() {
        let pkg = test_pkg(vec!["logr.go".to_string(), "slogr.go".to_string()]);
        let spec = build_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("/home/user/go/pkg/mod/github.com/go-logr/logr@v1.4.2/logr.go"),
            "compile script must use absolute GOMODCACHE paths: {run}"
        );
        assert!(
            run.contains("/home/user/go/pkg/mod/github.com/go-logr/logr@v1.4.2/slogr.go"),
            "compile script must include all go_files: {run}"
        );
    }

    #[test]
    fn test_run_no_list_src() {
        let spec = build_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            !run.contains("LIST_SRC"),
            "thirdparty compile script must not use LIST_SRC: {run}"
        );
    }

    #[test]
    fn test_deps_has_go_bin_no_empty_group() {
        let spec = build_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(deps.contains_key("go_bin"), "deps must have go_bin group");
        assert!(
            !deps.contains_key(""),
            "thirdparty deps must not have empty group (no pluginfs src)"
        );
    }

    #[test]
    fn test_out_has_a_group() {
        let spec = build_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, TargetSpecValue::Map(m) if m.contains_key("a")));
    }

    #[test]
    fn test_run_uses_tool_compile_and_p_flag() {
        let spec = build_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(run.contains("tool compile"), "must use go tool compile");
        assert!(
            run.contains("-p \"github.com/go-logr/logr\""),
            "must set -p flag to import path: {run}"
        );
    }

    #[test]
    fn test_main_package_uses_p_main() {
        let mut pkg = test_pkg(vec!["main.go".to_string()]);
        pkg.import_path = "github.com/foo/cmd".to_string();
        pkg.name = Some("main".to_string());
        let spec = build_spec(
            test_addr(),
            &pkg,
            &test_factors(),
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
            "main package must use -p \"main\": {run}"
        );
    }

    #[test]
    fn test_transitive_dep_in_importcfg() {
        let dep_addr = Addr {
            package: PkgBuf::from("@heph/go/std/fmt"),
            name: "build_lib".to_string(),
            args: Default::default(),
        };
        let transitive_libs = vec![("fmt".to_string(), dep_addr)];
        let spec = build_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &transitive_libs,
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("packagefile fmt="),
            "importcfg must include transitive dep: {run}"
        );
        let deps = match spec.config.get("deps").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(deps.contains_key("lib_fmt"), "deps must have lib_fmt group");
    }

    #[test]
    fn test_no_s_files_no_asm_steps() {
        let spec = build_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            !run.contains("tool asm"),
            "no .s files → must not emit tool asm: {run}"
        );
        assert!(
            !run.contains("tool pack"),
            "no .s files → must not emit tool pack: {run}"
        );
        assert!(
            !run.contains("-asmhdr"),
            "no .s files → must not emit -asmhdr: {run}"
        );
    }

    #[test]
    fn test_s_files_run_contains_asm_and_pack() {
        let pkg = test_pkg_with_asm(
            vec!["impl.go".to_string()],
            vec!["impl_amd64.s".to_string(), "util.s".to_string()],
        );
        let spec = build_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("tool asm"),
            "must invoke go tool asm for .s files: {run}"
        );
        assert!(
            run.contains("tool pack r"),
            "must pack .o files into archive: {run}"
        );
        assert!(
            run.contains("-asmhdr \"go_asm.h\""),
            "compile step must emit asm header: {run}"
        );
        assert!(
            run.contains("impl_amd64.s"),
            "must assemble impl_amd64.s: {run}"
        );
        assert!(run.contains("util.s"), "must assemble util.s: {run}");
        assert!(
            run.contains("impl_amd64.o"),
            "must pack impl_amd64.o: {run}"
        );
        assert!(run.contains("util.o"), "must pack util.o: {run}");
    }

    #[test]
    fn test_s_files_asm_uses_goroot_include() {
        let pkg = test_pkg_with_asm(vec!["impl.go".to_string()], vec!["impl.s".to_string()]);
        let spec = build_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("$GOROOT/pkg/include"),
            "asm step must include GOROOT/pkg/include: {run}"
        );
        assert!(
            run.contains("GOOS_linux"),
            "asm step must define GOOS_linux: {run}"
        );
        assert!(
            run.contains("GOARCH_amd64"),
            "asm step must define GOARCH_amd64: {run}"
        );
    }

    #[test]
    fn test_s_files_asm_uses_pkg_dir_include() {
        let pkg = test_pkg_with_asm(vec!["impl.go".to_string()], vec!["impl.s".to_string()]);
        let spec = build_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let run = match spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("/home/user/go/pkg/mod/github.com/go-logr/logr@v1.4.2"),
            "asm step must include the package dir: {run}"
        );
    }
}
