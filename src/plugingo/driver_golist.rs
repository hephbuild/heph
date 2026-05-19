use crate::debug_hash::DebugHasher;
use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
use crate::engine::driver::targetdef::{Input, InputMode, Output, TargetDef};
use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use crate::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use crate::hasync::Cancellable;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::{parse_map_string_strings, parse_string, parse_strings};
use crate::plugingo::pkg_analysis::{
    GoPackage, encode_go_package, encode_package_addrs, resolve_package_addrs,
};
use anyhow::Context;
use async_trait::async_trait;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

pub struct GoGolistDriver {
    pub go_bin_addr: String,
}

impl GoGolistDriver {
    pub fn new(go_bin_addr: impl Into<String>) -> Self {
        Self {
            go_bin_addr: go_bin_addr.into(),
        }
    }
}

#[derive(Clone, serde::Serialize)]
struct GoGolistDef {
    import_path: String,
    goos: String,
    goarch: String,
    goroot: String,
    build_tags: Vec<String>,
    dep_inputs: Vec<Input>,
}

/// Bump to invalidate every cached `_golist` artifact whenever the driver's
/// output format (package.bin layout, package_addrs.bin schema, …) changes.
const GO_GOLIST_FORMAT_VERSION: u32 = 5;

impl Hash for GoGolistDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        GO_GOLIST_FORMAT_VERSION.hash(state);
        self.import_path.hash(state);
        self.goos.hash(state);
        self.goarch.hash(state);
        self.goroot.hash(state);
        self.build_tags.hash(state);
        self.dep_inputs.hash(state);
        // go_bin input excluded (added at runtime, not hashed here — engine hashes content)
    }
}

#[async_trait]
impl ManagedDriver for GoGolistDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "go_golist".to_string(),
        })
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pkg = req.target_spec.addr.package.clone();
        let pkg_str = pkg.as_str();
        let config = &req.target_spec.config;

        let import_path = config
            .get("import_path")
            .and_then(|v| parse_string(v).ok().flatten())
            .ok_or_else(|| anyhow::anyhow!("go_golist: missing import_path"))?;

        let goos = config
            .get("goos")
            .and_then(|v| parse_string(v).ok().flatten())
            .ok_or_else(|| anyhow::anyhow!("go_golist: missing goos"))?;

        let goarch = config
            .get("goarch")
            .and_then(|v| parse_string(v).ok().flatten())
            .ok_or_else(|| anyhow::anyhow!("go_golist: missing goarch"))?;

        let goroot = config
            .get("goroot")
            .and_then(|v| parse_string(v).ok().flatten())
            .ok_or_else(|| anyhow::anyhow!("go_golist: missing goroot"))?;

        let build_tags = config
            .get("build_tags")
            .map(|v| parse_strings(v).context("parse build_tags"))
            .transpose()?
            .unwrap_or_default();

        // Parse deps (srcfiles, modfiles) — no go_bin here
        let dep_strings = config
            .get("deps")
            .map(|v| parse_map_string_strings(v).context("parse deps"))
            .transpose()?
            .unwrap_or_default();

        let mut dep_groups: Vec<&String> = dep_strings.keys().collect();
        dep_groups.sort();
        let mut dep_inputs: Vec<Input> = Vec::new();
        for group in dep_groups {
            let addrs = dep_strings.get(group).expect("group key from same map");
            for (i, addr_str) in addrs.iter().enumerate() {
                dep_inputs.push(Input {
                    r#ref: TargetAddr::parse(addr_str, &pkg)
                        .with_context(|| format!("parse dep addr {addr_str}"))?,
                    mode: InputMode::Standard,
                    origin_id: format!("dep|{group}|{i}"),
                });
            }
        }

        // Add go binary input inline — not declared in spec
        let go_bin_input = Input {
            r#ref: TargetAddr::parse(&self.go_bin_addr, &PkgBuf::from(""))
                .with_context(|| format!("parse go_bin_addr {}", self.go_bin_addr))?,
            mode: InputMode::Tool,
            origin_id: "go_bin".to_string(),
        };

        let def = GoGolistDef {
            import_path,
            goos,
            goarch,
            goroot,
            build_tags,
            dep_inputs: dep_inputs.clone(),
        };

        let hash = {
            let mut h = DebugHasher::new(
                Xxh3Default::new(),
                &format!("go_golist_{}", req.target_spec.addr.format()),
            );
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        // Parse outputs from spec, prepend package path
        let out_strings = config
            .get("out")
            .map(|v| parse_map_string_strings(v).context("parse out"))
            .transpose()?
            .unwrap_or_default();

        let outputs = out_strings
            .iter()
            .map(|(group, paths)| Output {
                group: group.clone(),
                paths: paths
                    .iter()
                    .map(|p| {
                        let full_path = if pkg_str.is_empty() {
                            p.clone()
                        } else {
                            format!("{pkg_str}/{p}")
                        };
                        Path {
                            content: Content::FilePath(full_path),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }
                    })
                    .collect(),
            })
            .collect();

        let mut all_inputs = dep_inputs;
        all_inputs.push(go_bin_input);

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: all_inputs,
                outputs,
                support_files: vec![],
                cache: true,
                disable_remote_cache: false,
                pty: false,
                hash,
                transparent: false,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def::<GoGolistDef>();

        // Find go binary from the go_bin tool input list
        let go_bin = {
            let managed = req
                .inputs
                .iter()
                .find(|m| m.input.origin_id == "go_bin")
                .ok_or_else(|| anyhow::anyhow!("go_golist: go_bin input not found"))?;
            let f = std::fs::File::open(&managed.list_path)
                .with_context(|| format!("open go_bin list {:?}", managed.list_path))?;
            std::io::BufReader::new(f)
                .lines()
                .find(|l| l.as_ref().is_ok_and(|s| !s.is_empty()))
                .ok_or_else(|| anyhow::anyhow!("go_bin list is empty"))??
        };

        // Build env from def fields + process environment for runtime vars
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("GOOS".to_string(), def.goos.clone());
        env.insert("GOARCH".to_string(), def.goarch.clone());
        env.insert("GOROOT".to_string(), def.goroot.clone());
        for name in &[
            "GOPATH",
            "GOMODCACHE",
            "GOCACHE",
            "HOME",
            "GONOSUMDB",
            "GOFLAGS",
            "GOPRIVATE",
        ] {
            if let Ok(v) = std::env::var(name) {
                env.insert(name.to_string(), v);
            }
        }

        let mut cmd_args = vec![
            "list".to_string(),
            "-json=Dir,ImportPath,Name,GoFiles,TestGoFiles,XTestGoFiles,EmbedPatterns,EmbedFiles,TestEmbedPatterns,TestEmbedFiles,XTestEmbedPatterns,XTestEmbedFiles,Imports,TestImports,XTestImports,Standard,Module,Match,Incomplete,Error".to_string(),
            "-e".to_string(),
            // -test populates TestEmbedFiles / XTestEmbedFiles (and resolves test
            // imports). Without it go list reports test embed patterns but never
            // resolves them, so the test embedcfg path globs against an empty
            // staged-file set. The flag also adds synthetic `pkg.test` / `pkg
            // [pkg.test]` / `pkg_test [pkg.test]` entries — we discard them
            // below and keep only the plain `pkg` entry.
            "-test".to_string(),
        ];

        if !def.build_tags.is_empty() {
            cmd_args.push(format!("-tags={}", def.build_tags.join(",")));
        }

        cmd_args.push(def.import_path.clone());

        let child = tokio::process::Command::new(&go_bin)
            .args(&cmd_args)
            .current_dir(&req.sandbox_pkg_dir)
            .env_clear()
            .envs(&env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn go list for {}", def.import_path))?;

        let output = tokio::select! {
            _ = ctoken.cancelled() => {
                anyhow::bail!("go list cancelled");
            }
            res = child.wait_with_output() => {
                res.context("wait for go list")?
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("go list failed for {}: {}", def.import_path, stderr);
        }

        // Parse once, normalize Dir on the struct, then serialize package.bin and
        // reuse the same value for addr resolution below.
        //
        // `-test` makes go list emit synthetic variants (`pkg.test`, `pkg [pkg.test]`,
        // `pkg_test [pkg.test]`); we keep only the canonical entry whose ImportPath
        // matches the request — that one has the resolved TestEmbedFiles /
        // XTestEmbedFiles we actually need downstream.
        let ws_prefix = req.sandbox_ws_dir.to_string_lossy();
        let mut pkgs: Vec<GoPackage> = serde_json::Deserializer::from_slice(&output.stdout)
            .into_iter::<GoPackage>()
            .collect::<Result<_, _>>()
            .context("parse go list output")?;
        pkgs.retain(|p| p.import_path == def.import_path);
        for p in &mut pkgs {
            if let Some(dir) = &p.dir {
                p.dir = Some(normalize_dir(dir, &ws_prefix));
            }
        }

        let pkg = pkgs.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("go list returned no entry matching {}", def.import_path)
        })?;

        let package_bin = encode_go_package(&pkg).context("encode package.bin")?;
        std::fs::write(req.sandbox_pkg_dir.join("package.bin"), &package_bin)
            .context("write package.bin")?;

        let source_map_path = req.sandbox_pkg_dir.join("source_map.json");
        let source_map: HashMap<String, String> = if source_map_path.exists() {
            let raw = std::fs::read_to_string(&source_map_path)
                .with_context(|| format!("read {source_map_path:?}"))?;
            serde_json::from_str(&raw).with_context(|| "parse source_map.json")?
        } else {
            HashMap::new()
        };

        let pkg_str = req.request.target.addr.package.as_str();
        let addrs = resolve_package_addrs(&pkg, pkg_str, &source_map);
        let addrs_bin = encode_package_addrs(&addrs).context("encode package_addrs.bin")?;
        std::fs::write(req.sandbox_pkg_dir.join("package_addrs.bin"), &addrs_bin)
            .context("write package_addrs.bin")?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        anyhow::bail!("run_shell not implemented for go_golist")
    }
}

/// Strip `ws_prefix/` from a go list `Dir` path so it becomes repo-root-relative.
fn normalize_dir(dir: &str, ws_prefix: &str) -> String {
    let prefix_slash = format!("{ws_prefix}/");
    if let Some(rel) = dir.strip_prefix(&prefix_slash) {
        rel.to_string()
    } else if dir == ws_prefix {
        String::new()
    } else {
        dir.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;

    fn driver() -> GoGolistDriver {
        GoGolistDriver::new("//@heph/bin:go")
    }

    fn make_parse_request(
        pkg: &str,
        import_path: &str,
        extra_deps: Vec<(&str, Vec<&str>)>,
    ) -> ParseRequest {
        use crate::engine::provider::TargetSpec;
        use crate::htaddr::Addr;
        use crate::loosespecparser::TargetSpecValue;
        use std::collections::HashMap;

        let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
        config.insert(
            "import_path".to_string(),
            TargetSpecValue::String(import_path.to_string()),
        );
        config.insert(
            "goos".to_string(),
            TargetSpecValue::String("linux".to_string()),
        );
        config.insert(
            "goarch".to_string(),
            TargetSpecValue::String("amd64".to_string()),
        );
        config.insert(
            "goroot".to_string(),
            TargetSpecValue::String("/usr/local/go".to_string()),
        );
        config.insert(
            "out".to_string(),
            TargetSpecValue::Map(HashMap::from([
                (
                    "pkg".to_string(),
                    TargetSpecValue::List(vec![TargetSpecValue::String("package.bin".to_string())]),
                ),
                (
                    "addrs".to_string(),
                    TargetSpecValue::List(vec![TargetSpecValue::String(
                        "package_addrs.bin".to_string(),
                    )]),
                ),
            ])),
        );

        if !extra_deps.is_empty() {
            let deps_map: HashMap<String, TargetSpecValue> = extra_deps
                .into_iter()
                .map(|(k, vs)| {
                    (
                        k.to_string(),
                        TargetSpecValue::List(
                            vs.into_iter()
                                .map(|v| TargetSpecValue::String(v.to_string()))
                                .collect(),
                        ),
                    )
                })
                .collect();
            config.insert("deps".to_string(), TargetSpecValue::Map(deps_map));
        }

        ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: Addr::new(PkgBuf::from(pkg), "_golist".to_string(), Default::default()),
                driver: "go_golist".to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            }),
        }
    }

    fn noop_ctoken() -> crate::hasync::StdCancellationToken {
        crate::hasync::StdCancellationToken::new()
    }

    #[tokio::test]
    async fn test_driver_name_is_go_golist() {
        let resp = driver().config(ConfigRequest {}).unwrap();
        assert_eq!(resp.name, "go_golist");
    }

    #[tokio::test]
    async fn test_parse_missing_import_path_errors() {
        use crate::engine::provider::TargetSpec;
        use crate::htaddr::Addr;
        use std::collections::HashMap;

        let ct = noop_ctoken();
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("mylib"),
                    "_golist".to_string(),
                    Default::default(),
                ),
                driver: "go_golist".to_string(),
                config: HashMap::new(),
                labels: vec![],
                transitive: Default::default(),
            }),
        };
        assert!(driver().parse(req, &ct).await.is_err());
    }

    #[tokio::test]
    async fn test_parse_includes_go_bin_input() {
        let ct = noop_ctoken();
        let req = make_parse_request("mylib", "example.com/mylib", vec![]);
        let resp = driver().parse(req, &ct).await.unwrap();
        assert!(
            resp.target_def
                .inputs
                .iter()
                .any(|i| i.origin_id == "go_bin"),
            "go_bin input must be present"
        );
    }

    #[tokio::test]
    async fn test_parse_go_bin_is_tool_mode() {
        let ct = noop_ctoken();
        let req = make_parse_request("mylib", "example.com/mylib", vec![]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let go_bin = resp
            .target_def
            .inputs
            .iter()
            .find(|i| i.origin_id == "go_bin")
            .unwrap();
        assert!(
            go_bin.mode == InputMode::Tool,
            "go_bin input must be Tool mode"
        );
    }

    #[tokio::test]
    async fn test_parse_dep_inputs_present() {
        let ct = noop_ctoken();
        let req = make_parse_request(
            "mylib",
            "example.com/mylib",
            vec![
                ("modfiles", vec!["//go.mod:_go_mod"]),
                ("srcfiles", vec!["//mylib:_go_src"]),
            ],
        );
        let resp = driver().parse(req, &ct).await.unwrap();
        let inputs = &resp.target_def.inputs;
        assert!(inputs.iter().any(|i| i.origin_id == "dep|modfiles|0"));
        assert!(inputs.iter().any(|i| i.origin_id == "dep|srcfiles|0"));
    }

    // Regression: dep groups came from a HashMap, so dep_inputs Vec order varied
    // across runs, making def.hash unstable and breaking the local cache for
    // go_golist targets. Order must now be sorted by group name.
    #[tokio::test]
    async fn test_parse_dep_inputs_order_is_deterministic() {
        let ct = noop_ctoken();
        let mk = || {
            make_parse_request(
                "mylib",
                "example.com/mylib",
                vec![
                    ("zsrc", vec!["//mylib:_go_src"]),
                    ("modfiles", vec!["//go.mod:_go_mod"]),
                    ("asrc", vec!["//mylib:_extra"]),
                ],
            )
        };
        let resp_a = driver().parse(mk(), &ct).await.unwrap();
        let resp_b = driver().parse(mk(), &ct).await.unwrap();

        let ids_a: Vec<&str> = resp_a
            .target_def
            .inputs
            .iter()
            .filter(|i| i.origin_id.starts_with("dep|"))
            .map(|i| i.origin_id.as_str())
            .collect();
        let ids_b: Vec<&str> = resp_b
            .target_def
            .inputs
            .iter()
            .filter(|i| i.origin_id.starts_with("dep|"))
            .map(|i| i.origin_id.as_str())
            .collect();

        assert_eq!(ids_a, ids_b, "dep input order must be stable across parses");
        // Verify it's sorted by group name (asrc < modfiles < zsrc).
        assert_eq!(ids_a, vec!["dep|asrc|0", "dep|modfiles|0", "dep|zsrc|0"]);
        assert_eq!(resp_a.target_def.hash, resp_b.target_def.hash);
    }

    #[tokio::test]
    async fn test_parse_outputs_prepend_package() {
        let ct = noop_ctoken();
        let req = make_parse_request("mylib", "example.com/mylib", vec![]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let pkg_out = resp
            .target_def
            .outputs
            .iter()
            .find(|o| o.group == "pkg")
            .unwrap();
        assert!(pkg_out.paths.iter().any(|p| matches!(
            &p.content,
            Content::FilePath(s) if s == "mylib/package.bin"
        )));
    }

    #[tokio::test]
    async fn test_parse_outputs_no_prefix_for_empty_package() {
        let ct = noop_ctoken();
        let req = make_parse_request("", "example.com/mylib", vec![]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let pkg_out = resp
            .target_def
            .outputs
            .iter()
            .find(|o| o.group == "pkg")
            .unwrap();
        assert!(pkg_out.paths.iter().any(|p| matches!(
            &p.content,
            Content::FilePath(s) if s == "package.bin"
        )));
    }

    #[test]
    fn test_normalize_dir_strips_ws_prefix() {
        assert_eq!(normalize_dir("/sandbox/ws/mylib", "/sandbox/ws"), "mylib");
    }

    #[test]
    fn test_normalize_dir_equals_ws_prefix_yields_empty() {
        assert_eq!(normalize_dir("/sandbox/ws", "/sandbox/ws"), "");
    }

    #[test]
    fn test_normalize_dir_leaves_third_party_unchanged() {
        let dir = "/home/user/go/pkg/mod/github.com/foo/bar@v1.0.0";
        assert_eq!(normalize_dir(dir, "/sandbox/ws"), dir);
    }
}
