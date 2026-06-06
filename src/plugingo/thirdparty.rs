use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::htvalue::Value;
use crate::plugingo::addr_util::{
    go_bin_tools_config, import_path_to_dep_group, to_run_value, write_importcfg_script,
};
use crate::plugingo::factors::Factors;
use crate::plugingo::pkg_analysis::GoPackage;
use crate::plugingo::target_std::archive_filename;
use std::collections::{BTreeMap, HashMap};

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
    go_bin_addr: &str,
    goroot: &str,
) -> TargetSpec {
    let mod_at_ver = format!("{module}@{version}");
    // Multi-line bash. `awk` parses `Dir` out of the JSON `go mod download`
    // emits. The stub go.mod blocks Go from reading the project go.mod.
    //
    // `cp` choice: BSD/macOS cp copies xattrs (com.apple.quarantine on
    // Go's mod cache files) by default, which fails with EPERM on FUSE
    // overlays that reject xattr writes. `cp -X` on macOS suppresses
    // xattr copy; on GNU cp `-X` means `--one-file-system`, so branch
    // on `uname`.
    let run = vec![
        "printf 'module heph_ignore\\n' > go.mod".to_string(),
        format!("\"$TOOL_GO\" mod download -modcacherw -json '{mod_at_ver}' > mod.json"),
        "rm go.mod".to_string(),
        "MOD_DIR=$(awk -F'\"' '/\"Dir\": / { print $4 }' mod.json)".to_string(),
        // One compound statement — keep its lines in a single entry.
        "if [ -z \"$MOD_DIR\" ]; then\n\
           echo 'go mod download produced no Dir' >&2\n\
           cat mod.json >&2\n\
           exit 1\n\
         fi"
        .to_string(),
        "if [ \"$(uname)\" = Darwin ]; then CP=\"cp -RX\"; else CP=\"cp -R\"; fi".to_string(),
        "$CP \"$MOD_DIR/.\" .".to_string(),
        "rm mod.json".to_string(),
        "chmod -R u+w .".to_string(),
    ];

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("run".to_string(), to_run_value(run));
    config.insert("tools".to_string(), go_bin_tools_config(go_bin_addr));
    // Glob form (contains `*`) — pluginexec packs every matching file under
    // the target's pkg into the artifact, preserving relative paths.
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            String::new(),
            Value::List(vec![Value::String("**/*".to_string())]),
        )])),
    );
    config.insert(
        "runtime_env".to_string(),
        Value::Map(HashMap::from([(
            "GOROOT".to_string(),
            Value::String(goroot.to_string()),
        )])),
    );
    // CGO pin lives in `env` (hashed; pluginexec/mod.rs:70 excludes runtime_env
    // from the def hash). `go mod download` itself does not invoke the C
    // toolchain, but pinning here keeps the global rule simple: no Go process
    // in heph ever autodetects CGO.
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
    // GOPROXY/GOMODCACHE/etc must be inherited at runtime so network fetches
    // and modcache placement match the user's host config.
    config.insert(
        "runtime_pass_env".to_string(),
        Value::List(
            [
                "HOME",
                "GOMODCACHE",
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
            .map(|s| Value::String((*s).to_string()))
            .collect(),
        ),
    );

    TargetSpec {
        addr,
        driver: "sh".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

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
    embed_addr: Option<&Addr>,
    embed_file_addrs: &[String],
    go_bin_addr: &str,
    goroot: &str,
    gocache: &str,
) -> TargetSpec {
    let import_path = &pkg.import_path;
    let package_name = pkg.name.as_deref().unwrap_or("");

    let out_file = archive_filename(import_path);
    // Go requires main packages to be compiled with -p "main".
    let p_flag = if package_name == "main" {
        "main"
    } else {
        import_path.as_str()
    };

    let has_asm = !pkg.s_files.is_empty();
    let run = generate_compile_script(
        p_flag,
        transitive_libs,
        &out_file,
        &pkg.s_files,
        factors,
        embed_addr.is_some(),
    );

    let mut deps: BTreeMap<String, Value> = transitive_libs
        .iter()
        .map(|(dep_import_path, dep_addr)| {
            let group = import_path_to_dep_group(dep_import_path);
            (group, Value::List(vec![Value::String(dep_addr.format())]))
        })
        .collect();
    deps.insert(
        String::new(),
        Value::List(src_addrs.iter().map(|s| Value::String(s.clone())).collect()),
    );
    if has_asm {
        deps.insert(
            "asm".to_string(),
            Value::List(
                s_file_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
        // .h headers next to .s files (e.g. purego ships its own abi_arm64.h).
        // Staged so asm `-I .` can resolve `#include "abi_arm64.h"`.
        if !h_file_addrs.is_empty() {
            deps.insert(
                "hdr".to_string(),
                Value::List(
                    h_file_addrs
                        .iter()
                        .map(|s| Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
    }
    if let Some(e) = embed_addr {
        deps.insert(
            "embed".to_string(),
            Value::List(vec![Value::String(e.format())]),
        );
    }
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
    config.insert("run".to_string(), to_run_value(run));
    config.insert("tools".to_string(), go_bin_tools_config(go_bin_addr));
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
            // go tool {compile,asm,link} initialize Go's build cache layer on
            // startup (for build IDs). Without GOCACHE or $HOME they error
            // with "build cache is required, but could not be located".
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

fn generate_compile_script(
    p_flag: &str,
    transitive_libs: &[(String, Addr)],
    out_file: &str,
    s_files: &[String],
    factors: &Factors,
    has_embed: bool,
) -> Vec<String> {
    let mut lines = write_importcfg_script(transitive_libs, None);

    let has_asm = !s_files.is_empty();
    let asmhdr_flag = if has_asm { " -asmhdr \"go_asm.h\"" } else { "" };
    // Go 1.17+ uses ABIInternal for Go-defined funcs but asm TEXT directives
    // emit ABI0 symbols. Without -symabis the compiler emits ABIInternal calls
    // to asm-defined funcs, producing "relocation target X not defined" at
    // link time. -gensymabis declares the ABI of asm symbols so the compiler
    // can match them.
    let symabis_flag = if has_asm { " -symabis \"symabis\"" } else { "" };
    let embedcfg_flag = if has_embed {
        " -embedcfg \"$SRC_EMBED\""
    } else {
        ""
    };

    let goos = &factors.goos;
    let goarch = &factors.goarch;

    // -shared = position-independent code. macOS arm64 binaries default to
    // PIE, so Go's own build pipeline passes -shared to BOTH compile and asm.
    // Without it, asm symbols may use wrong relocation modes and link fails
    // with "relocation target X not defined".
    let shared = " -shared";

    if has_asm {
        // Step 0: create empty go_asm.h so gensymabis can parse .s files that
        // `#include "go_asm.h"` (chacha20, runtime helpers, etc.). Go's own
        // build pipeline does the same (`echo -n > go_asm.h` before
        // -gensymabis). Without this the asm parse silently emits an empty
        // symabis file → compiler uses ABIInternal for asm-defined funcs →
        // linker fails with "relocation target X not defined".
        lines.push("touch go_asm.h".to_string());

        // Step 1: generate symabis from all .s files (one invocation).
        let s_files_list = s_files
            .iter()
            .map(|f| format!("\"./{}\"", f))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!(
            "\"$TOOL_GO\" tool asm -p \"{p_flag}\" -trimpath=\"$WORKSPACE_ROOT\" -I . -I \"$GOROOT/pkg/include\" -D GOOS_{goos} -D GOARCH_{goarch}{shared} -gensymabis -o \"symabis\" {s_files_list}"
        ));
    }

    // Step 2: compile .go files. With -symabis the compiler learns asm ABIs;
    // -asmhdr writes go_asm.h into CWD for the asm step to pick up via `-I .`.
    // Sources are staged into the consumer's sandbox by the engine (via the
    // download target's filtered outputs). `@${LIST_SRC}` is a response file
    // listing every staged source — same convention as first-party.
    lines.push(format!(
        "\"$TOOL_GO\" tool compile -p \"{p_flag}\" -trimpath=\"$WORKSPACE_ROOT\" -pack -importcfg \"$importcfg\"{symabis_flag}{asmhdr_flag}{embedcfg_flag}{shared} -o \"{out_file}\" \"@${{LIST_SRC}}\"",
    ));

    if has_asm {
        // Step 3: assemble each .s file. -I . finds go_asm.h written above.
        for s_file in s_files {
            let obj_file = format!("{}.o", s_file.trim_end_matches(".s"));
            lines.push(format!(
                "\"$TOOL_GO\" tool asm -p \"{p_flag}\" -trimpath=\"$WORKSPACE_ROOT\" -I . -I \"$GOROOT/pkg/include\" -D GOOS_{goos} -D GOARCH_{goarch}{shared} -o \"{obj_file}\" \"./{s_file}\""
            ));
        }
        // Step 4: pack asm objs into the archive.
        let obj_args = s_files
            .iter()
            .map(|f| format!("\"{}.o\"", f.trim_end_matches(".s")))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!(
            "\"$TOOL_GO\" tool pack r \"{out_file}\" {obj_args}"
        ));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;
    use crate::plugingo::pkg_analysis::{GoModule, GoPackage};

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
            env: Default::default(),
            ldflags: vec![],
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

    #[test]
    fn test_download_spec_driver() {
        let spec = build_download_spec(
            download_addr(),
            "github.com/go-logr/logr",
            "v1.4.2",
            "//@heph/bin:go",
            "/usr/local/go",
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_download_spec_run_contains_go_mod_download() {
        let spec = build_download_spec(
            download_addr(),
            "github.com/go-logr/logr",
            "v1.4.2",
            "//@heph/bin:go",
            "/usr/local/go",
        );
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
        assert!(run.contains("$TOOL_GO"), "must call $TOOL_GO: {run}");
    }

    #[test]
    fn test_download_spec_out_is_glob() {
        let spec = build_download_spec(
            download_addr(),
            "github.com/go-logr/logr",
            "v1.4.2",
            "//@heph/bin:go",
            "/usr/local/go",
        );
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
    fn test_download_spec_has_go_tool_no_deps() {
        let spec = build_download_spec(
            download_addr(),
            "github.com/go-logr/logr",
            "v1.4.2",
            "//@heph/bin:go",
            "/usr/local/go",
        );
        let tools = match spec.config.get("tools").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(tools.contains_key("go"));
        assert_eq!(tools.len(), 1, "download must only have go tool");
        assert!(
            !spec.config.contains_key("deps"),
            "download must not declare any deps"
        );
    }

    // ---- build_lib_spec ----

    #[test]
    fn test_download_spec_env_pins_cgo_disabled() {
        let spec = build_download_spec(
            download_addr(),
            "github.com/go-logr/logr",
            "v1.4.2",
            "//@heph/bin:go",
            "/usr/local/go",
        );
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

    #[test]
    fn test_build_lib_env_pins_cgo_disabled() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let env = match spec.config.get("env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(env.get("CGO_ENABLED"), Some(Value::String(s)) if s == "0"),
            "thirdparty build_lib env must pin CGO_ENABLED=0 in the hashed map: {:?}",
            env.get("CGO_ENABLED")
        );
    }

    #[test]
    fn test_build_lib_driver_is_bash() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        assert_eq!(spec.driver, "sh");
    }

    #[test]
    fn test_build_lib_has_go_build_label() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        assert!(
            spec.labels.contains(&"go-build".to_string()),
            "thirdparty build_lib spec must carry go-build label: {:?}",
            spec.labels
        );
    }

    #[test]
    fn test_build_lib_run_uses_list_src() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        assert!(
            run.contains("@${LIST_SRC}"),
            "run must use @${{LIST_SRC}}: {run}"
        );
        assert!(
            run.contains("tool compile"),
            "must call tool compile: {run}"
        );
    }

    #[test]
    fn test_build_lib_no_gomodcache_paths_in_run() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        // Reject host GOMODCACHE leakage — sources must flow through sandbox staging.
        assert!(
            !run.contains("/go/pkg/mod/"),
            "compile script must not embed host GOMODCACHE paths: {run}"
        );
        assert!(
            !run.contains("/home/user/"),
            "compile script must not embed host paths: {run}"
        );
    }

    #[test]
    fn test_build_lib_src_addrs_in_default_group() {
        let src = filter_src_addr("logr.go");
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[src.clone()],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        let default_group = match deps.get("").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(matches!(&default_group[0], Value::String(s) if s == &src));
    }

    #[test]
    fn test_build_lib_tools_has_go() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let tools = match spec.config.get("tools").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(tools.contains_key("go"));
    }

    #[test]
    fn test_build_lib_main_package_uses_p_main() {
        let mut pkg = test_pkg(vec!["main.go".to_string()]);
        pkg.import_path = "github.com/foo/cmd".to_string();
        pkg.name = Some("main".to_string());
        let spec = build_lib_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            &[filter_src_addr("main.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        assert!(
            run.contains("-p \"main\""),
            "main package must use -p \"main\": {run}"
        );
    }

    #[test]
    fn test_build_lib_non_main_uses_import_path() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        assert!(
            run.contains("-p \"github.com/go-logr/logr\""),
            "non-main pkg must use -p <import>: {run}"
        );
    }

    #[test]
    fn test_build_lib_transitive_dep_in_importcfg() {
        let dep_addr = Addr::new(
            PkgBuf::from("@heph/go/std/fmt"),
            "build_lib".to_string(),
            Default::default(),
        );
        let transitive_libs = vec![("fmt".to_string(), dep_addr)];
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &transitive_libs,
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        assert!(
            run.contains("packagefile fmt="),
            "importcfg dep missing: {run}"
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(deps.contains_key("lib_fmt"));
    }

    #[test]
    fn test_build_lib_out_has_a_group() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let out = spec.config.get("out").unwrap();
        assert!(matches!(out, Value::Map(m) if m.contains_key("a")));
    }

    #[test]
    fn test_build_lib_s_files_run_contains_asm_and_pack() {
        let pkg = test_pkg_with_asm(
            vec!["impl.go".to_string()],
            vec!["impl_amd64.s".to_string(), "util.s".to_string()],
        );
        let s_addrs = vec![filter_src_addr("impl_amd64.s"), filter_src_addr("util.s")];
        let spec = build_lib_spec(
            test_addr(),
            &pkg,
            &test_factors(),
            &[],
            &[filter_src_addr("impl.go")],
            &s_addrs,
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        assert!(run.contains("tool asm"), "must invoke go tool asm: {run}");
        assert!(run.contains("tool pack r"), "must pack .o files: {run}");
        assert!(
            run.contains("-asmhdr \"go_asm.h\""),
            "compile must emit asm header: {run}"
        );
        // -gensymabis + -symabis are required so the Go compiler (ABIInternal)
        // can resolve calls to asm-defined funcs (ABI0). Without them, linker
        // fails with "relocation target X not defined".
        assert!(
            run.contains("-gensymabis"),
            "must run asm -gensymabis to declare asm symbol ABIs: {run}"
        );
        assert!(
            run.contains("-symabis \"symabis\""),
            "compile must consume symabis file: {run}"
        );
        assert!(
            run.contains("impl_amd64.s"),
            "must assemble impl_amd64.s: {run}"
        );
        assert!(run.contains("util.s"), "must assemble util.s: {run}");
        // .s files must be referenced via CWD (sandbox_pkg_dir), not host GOMODCACHE.
        assert!(
            run.contains("\"./impl_amd64.s\""),
            "asm step must use ./<file> (CWD-relative): {run}"
        );
        assert!(
            !run.contains("/go/pkg/mod/"),
            "asm step must not embed host paths: {run}"
        );
        // Deps map must carry the .s file refs.
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("asm"),
            "deps must include asm group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_build_lib_no_s_files_no_asm_steps() {
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            None,
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        assert!(!run.contains("tool asm"), "no .s → no tool asm: {run}");
        assert!(!run.contains("tool pack"), "no .s → no tool pack: {run}");
        assert!(!run.contains("-asmhdr"), "no .s → no -asmhdr: {run}");
    }

    #[test]
    fn test_build_lib_with_embed_addr_adds_embedcfg() {
        let embed = Addr::new(
            PkgBuf::from("@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2"),
            "embed".to_string(),
            Default::default(),
        );
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            Some(&embed),
            &[],
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let run = run_str(&spec);
        assert!(run.contains("-embedcfg"), "must include -embedcfg: {run}");
        assert!(
            run.contains("$SRC_EMBED"),
            "must reference $SRC_EMBED: {run}"
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(deps.contains_key("embed"));
    }

    #[test]
    fn test_build_lib_embed_files_in_own_dep_group() {
        let embed = Addr::new(
            PkgBuf::from("@heph/go/thirdparty/github.com/go-logr/logr@v1.4.2"),
            "embed".to_string(),
            Default::default(),
        );
        let embed_files = vec![filter_src_addr("static/x.html")];
        let spec = build_lib_spec(
            test_addr(),
            &test_pkg(vec!["logr.go".to_string()]),
            &test_factors(),
            &[],
            &[filter_src_addr("logr.go")],
            &[],
            &[],
            Some(&embed),
            &embed_files,
            "//@heph/bin:go",
            "/usr/local/go",
            "/tmp/gocache",
        );
        let deps = match spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            deps.contains_key("embed_files"),
            "embed_files must be its own group: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }
}
