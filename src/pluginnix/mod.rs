use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path as TPath};
use crate::engine::driver::targetdef::{Input, InputMode, Output, TargetDef};
use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use crate::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use crate::hasync::Cancellable;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::{TargetSpecValue, parse_string, parse_strings};
use crate::proc_exec;
use anyhow::Context as _;
use async_trait::async_trait;
use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

pub const DRIVER_NAME: &str = "nix";

/// Bump to invalidate every cached nix-driver artifact whenever the def shape
/// or wrapper-script format changes.
///
/// v2: switched from one Output group with N FilePaths to N Output groups
/// each with 1 FilePath, so the tool can be referenced without a `|<program>`
/// selector. Cached v1 entries have a single-group shape that the new code
/// path treats as one binary — bump to force recompute.
const DRIVER_FORMAT_VERSION: u32 = 2;

const NIX_TOOL_ADDR: &str = "//@heph/bin:nix";

pub struct Driver {
    home_dir: PathBuf,
}

impl Driver {
    pub fn new(home_dir: PathBuf) -> Self {
        Self { home_dir }
    }
}

#[derive(Clone, serde::Serialize)]
struct NixDef {
    nixpkgs: String,
    /// sorted at parse time so hash is order-independent
    packages: Vec<String>,
    /// sorted at parse time so hash is order-independent
    programs: Vec<String>,
    system: String,
}

impl Hash for NixDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        DRIVER_FORMAT_VERSION.hash(state);
        self.nixpkgs.hash(state);
        self.packages.hash(state);
        self.programs.hash(state);
        self.system.hash(state);
    }
}

fn take_string(
    m: &mut HashMap<&str, &TargetSpecValue>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    match m.remove(key) {
        Some(v) => parse_string(v).with_context(|| format!("parse `{key}`")),
        None => Ok(None),
    }
}

fn take_strings(m: &mut HashMap<&str, &TargetSpecValue>, key: &str) -> anyhow::Result<Vec<String>> {
    match m.remove(key) {
        Some(v) => parse_strings(v).with_context(|| format!("parse `{key}`")),
        None => Ok(vec![]),
    }
}

/// Map host (ARCH, OS) to nix system triple. Unknown combos bail loudly so
/// def.hash always contains a concrete system — otherwise an x86_64-linux
/// build would cache-hit an aarch64-darwin entry and the wrapper would
/// exec the wrong-arch binary.
fn host_nix_system() -> anyhow::Result<String> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let nix_arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "x86" => "i686",
        other => anyhow::bail!("nix driver: unsupported host arch `{other}`"),
    };
    let nix_os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        other => anyhow::bail!("nix driver: unsupported host os `{other}`"),
    };
    Ok(format!("{nix_arch}-{nix_os}"))
}

/// Pass through the host env vars nix needs to:
/// - find the user store / channels / support tools (`HOME`, `USER`, `NIX_PATH`,
///   `XDG_CACHE_HOME`, `PATH`)
/// - validate TLS for flake fetches (`NIX_SSL_CERT_FILE`, `SSL_CERT_FILE`,
///   `SSL_CERT_DIR`, `CURL_CA_BUNDLE`) — needed when the host has a
///   non-default CA bundle (corporate MITM proxy, nixos system cert path, …)
/// - route through corporate proxies (`HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`
///   + lowercase variants; curl honors both)
fn passthrough_nix_env() -> Vec<(OsString, OsString)> {
    let mut out = Vec::new();
    for name in &[
        "HOME",
        "USER",
        "NIX_PATH",
        "XDG_CACHE_HOME",
        "PATH",
        "NIX_SSL_CERT_FILE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
    ] {
        if let Ok(v) = std::env::var(name) {
            out.push((OsString::from(name), OsString::from(v)));
        }
    }
    out
}

/// Resolve a (possibly mutable) flake URL to its locked form via
/// `nix flake metadata --json`. Output `.url` is the immutable URL containing
/// the resolved rev / narHash, suitable for `getFlake` in pure-eval mode.
async fn lock_flake_url(
    nix_bin: &str,
    url: &str,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<String> {
    let args: Vec<OsString> = [
        "--extra-experimental-features",
        "nix-command flakes",
        "flake",
        "metadata",
        "--json",
        "--no-update-lock-file",
        "--no-write-lock-file",
        url,
    ]
    .iter()
    .map(OsString::from)
    .collect();

    // setsid=false: nix is a single-process invocation; the supervisor's
    // direct-pid kill is sufficient on hard-shutdown.
    let spec = proc_exec::Spec {
        program: PathBuf::from(nix_bin),
        args,
        env: passthrough_nix_env(),
        cwd: std::env::current_dir().context("current_dir for nix flake metadata")?,
        stdin: proc_exec::StdioSpec::Null,
        stdout: proc_exec::StdioSpec::Piped,
        stderr: proc_exec::StdioSpec::Piped,
        setsid: false,
        ctty: false,
    };

    let output = proc_exec::output(spec, ctoken)
        .await
        .context("wait for nix flake metadata")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("nix flake metadata failed for {url}: {stderr}");
    }
    let v: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse nix flake metadata json")?;
    let locked = v
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("nix flake metadata json missing `url` field"))?;
    Ok(locked.to_string())
}

fn build_nix_expr(
    locked_flake_url: &str,
    system: &str,
    packages: &[String],
    env_name: &str,
) -> String {
    // packages spliced as raw nix; user controls the syntax. system quoted as a
    // single attr path component (e.g. legacyPackages."x86_64-linux") so triples
    // containing `-` parse correctly. Flake URL must be locked (contain rev /
    // narHash) so `getFlake` works without `--impure`.
    let paths = packages.join(" ");
    format!(
        "let flake = builtins.getFlake \"{}\"; pkgs = flake.legacyPackages.\"{}\"; \
         in pkgs.buildEnv {{ name = \"{}\"; paths = [ {} ]; }}",
        locked_flake_url, system, env_name, paths
    )
}

/// Sanitize an Addr's format into a string usable as a derivation `name` attr.
fn sanitize_env_name(addr_str: &str) -> String {
    let mut out = String::with_capacity(addr_str.len() + 6);
    out.push_str("rheph-");
    for c in addr_str.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let mut m: HashMap<&str, &TargetSpecValue> = req
            .target_spec
            .config
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect();

        let nixpkgs = take_string(&mut m, "nixpkgs")?
            .ok_or_else(|| anyhow::anyhow!("nix driver requires `nixpkgs`"))?;
        let mut packages = take_strings(&mut m, "packages")?;
        let mut programs = take_strings(&mut m, "programs")?;
        let system = match take_string(&mut m, "system")? {
            Some(s) => s,
            None => host_nix_system()?,
        };

        if packages.is_empty() {
            anyhow::bail!("nix driver requires `packages` (non-empty list)");
        }
        if programs.is_empty() {
            anyhow::bail!("nix driver requires `programs` (non-empty list)");
        }

        if !m.is_empty() {
            let unknown: Vec<&str> = m.into_keys().collect();
            anyhow::bail!("nix driver does not support config keys: {:?}", unknown);
        }

        // Sort so hash is order-independent. Sorting also keeps the generated
        // nix expression deterministic, which matters for nix's eval cache.
        packages.sort();
        programs.sort();

        let nix_tool = Input {
            r#ref: TargetAddr::parse(NIX_TOOL_ADDR, &PkgBuf::from(""))
                .with_context(|| format!("parse {NIX_TOOL_ADDR}"))?,
            mode: InputMode::Tool,
            origin_id: "nix".to_string(),
            annotations: std::collections::BTreeMap::from([(
                "unpack_root".to_string(),
                "tools".to_string(),
            )]),
        };

        // One Output group per program (group name = program name). Each output
        // has a single FilePath. Required because the engine's tool-input check
        // (`engine/link.rs`) refuses a tool target that resolves to more than
        // one FilePath across the selected output groups — so consumers must
        // pick a specific program via `tools = ["//pkg:env|<program>"]`.
        let pkg = req.target_spec.addr.package.as_str();
        let outputs: Vec<Output> = programs
            .iter()
            .map(|p| {
                let path = if pkg.is_empty() {
                    format!("bin/{p}")
                } else {
                    format!("{pkg}/bin/{p}")
                };
                Output {
                    group: p.clone(),
                    paths: vec![TPath {
                        content: Content::FilePath(path),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }
            })
            .collect();

        let def = NixDef {
            nixpkgs,
            packages,
            programs,
            system,
        };

        let hash = {
            let mut h = Xxh3Default::new();
            def.hash(&mut h);
            req.target_spec.addr.format().hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: vec![nix_tool],
                outputs,
                support_files: vec![],
                cache: true,
                // The wrapper points at host-local /nix/store paths; remote
                // consumers would not have those paths realized.
                disable_remote_cache: true,
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
        let def = req.request.target.def::<NixDef>();
        let addr_str = req.request.target.addr.format();

        // Resolve nix binary from the staged tool input list.
        let nix_bin = {
            use std::io::BufRead as _;
            let managed = req
                .inputs
                .iter()
                .find(|m| m.input.origin_id == "nix")
                .ok_or_else(|| anyhow::anyhow!("nix driver: `nix` tool input not found"))?;
            let list_path = managed.require_list_path()?;
            let f = std::fs::File::open(list_path)
                .with_context(|| format!("open nix tool list {:?}", list_path))?;
            std::io::BufReader::new(f)
                .lines()
                .find(|l| l.as_ref().is_ok_and(|s| !s.is_empty()))
                .ok_or_else(|| anyhow::anyhow!("nix driver: nix tool list is empty"))??
        };

        // gcroot symlink path. `nix build --out-link <p>` registers <p> as an
        // indirect auto-gcroot, so the resolved store path + its closure
        // survive `nix-collect-garbage` for as long as the symlink exists.
        let gcroots_dir = self.home_dir.join("nix-gcroots");
        tokio::fs::create_dir_all(&gcroots_dir)
            .await
            .with_context(|| format!("create gcroots dir {:?}", gcroots_dir))?;
        let gcroot_path = gcroots_dir.join(req.request.target.addr.hash_str());
        // `nix build --out-link` refuses to overwrite an existing path.
        match tokio::fs::remove_file(&gcroot_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("remove stale gcroot {:?}", gcroot_path));
            }
        }

        // Lock the user-supplied flake URL. `builtins.getFlake` refuses
        // unlocked (mutable) refs like `github:foo/bar/main` without `--impure`;
        // we deliberately don't use `--impure` because it disables the flake
        // eval cache (the single biggest avoidable eval cost — jvns).
        // `nix flake metadata --json <url>` resolves to an immutable URL
        // (containing rev / narHash) which `getFlake` accepts in pure mode.
        let locked_flake_url = lock_flake_url(&nix_bin, &def.nixpkgs, ctoken)
            .await
            .with_context(|| format!("lock flake url {}", def.nixpkgs))?;

        let env_name = sanitize_env_name(&addr_str);
        let expr = build_nix_expr(&locked_flake_url, &def.system, &def.packages, &env_name);

        let args: Vec<OsString> = [
            OsString::from("--extra-experimental-features"),
            OsString::from("nix-command flakes"),
            OsString::from("build"),
            OsString::from("--expr"),
            OsString::from(&expr),
            OsString::from("--out-link"),
            gcroot_path.clone().into_os_string(),
            OsString::from("--print-out-paths"),
            // jvns: skip lock-file churn when iterating locally.
            OsString::from("--no-update-lock-file"),
            OsString::from("--no-write-lock-file"),
            // Deliberately NOT `--impure`: disables the flake eval cache.
        ]
        .to_vec();

        // setsid=false: see comment in lock_flake_url for the rationale.
        let spec = proc_exec::Spec {
            program: PathBuf::from(&nix_bin),
            args,
            env: passthrough_nix_env(),
            cwd: req.sandbox_pkg_dir.clone(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };

        let output = proc_exec::output(spec, ctoken)
            .await
            .with_context(|| format!("wait for nix build for {addr_str}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("nix build failed for {addr_str}: {stderr}");
        }

        let store_path = std::str::from_utf8(&output.stdout)
            .context("nix build stdout not utf-8")?
            .trim()
            .to_string();
        if store_path.is_empty() {
            anyhow::bail!("nix build produced empty store path");
        }

        // Write one wrapper per program into the sandbox so the bridge can
        // collect them into the output tar.
        let bin_dir = req.sandbox_pkg_dir.join("bin");
        tokio::fs::create_dir_all(&bin_dir)
            .await
            .with_context(|| format!("create {:?}", bin_dir))?;

        for program in &def.programs {
            let target_bin = format!("{store_path}/bin/{program}");
            tokio::fs::metadata(&target_bin).await.with_context(|| {
                format!("program `{program}` not present at {target_bin} after nix build")
            })?;
            let wrapper = format!("#!/bin/sh\nexec {target_bin} \"$@\"\n");
            let wrapper_path = bin_dir.join(program);
            tokio::fs::write(&wrapper_path, wrapper.as_bytes())
                .await
                .with_context(|| format!("write wrapper {:?}", wrapper_path))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mut perm = tokio::fs::metadata(&wrapper_path).await?.permissions();
                perm.set_mode(0o755);
                tokio::fs::set_permissions(&wrapper_path, perm).await?;
            }
        }

        // The bridge appends the collected tar artifact based on `outputs[]`
        // with `collect: true`. Nothing to add here.
        Ok(ManagedRunResponse { artifacts: vec![] })
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        anyhow::bail!("run_shell not implemented for nix driver")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::provider::TargetSpec;
    use crate::hasync::StdCancellationToken;
    use crate::htaddr::parse_addr;
    use std::collections::HashMap;

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn driver() -> Driver {
        Driver::new(PathBuf::from("/tmp/nix-driver-test-home"))
    }

    fn nix_available() -> bool {
        std::process::Command::new("nix")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    macro_rules! require_nix {
        () => {
            if !nix_available() {
                eprintln!("skipping: nix not in PATH");
                return;
            }
        };
    }

    fn make_parse_req(addr: &str, config: HashMap<String, TargetSpecValue>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr(addr).unwrap(),
                driver: DRIVER_NAME.to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            }),
        }
    }

    fn basic_config() -> HashMap<String, TargetSpecValue> {
        HashMap::from([
            (
                "nixpkgs".to_string(),
                TargetSpecValue::String("github:NixOS/nixpkgs/abc".to_string()),
            ),
            (
                "packages".to_string(),
                TargetSpecValue::List(vec![TargetSpecValue::String("pkgs.ripgrep".to_string())]),
            ),
            (
                "programs".to_string(),
                TargetSpecValue::List(vec![TargetSpecValue::String("rg".to_string())]),
            ),
        ])
    }

    #[tokio::test]
    async fn parse_requires_nixpkgs() {
        require_nix!();
        let d = driver();
        let mut cfg = basic_config();
        cfg.remove("nixpkgs");
        let err = d
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .err()
            .expect("must error");
        assert!(err.to_string().contains("nixpkgs"), "{err}");
    }

    #[tokio::test]
    async fn parse_requires_packages() {
        require_nix!();
        let d = driver();
        let mut cfg = basic_config();
        cfg.remove("packages");
        let err = d
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .err()
            .expect("must error");
        assert!(err.to_string().contains("packages"), "{err}");
    }

    #[tokio::test]
    async fn parse_requires_programs() {
        require_nix!();
        let d = driver();
        let mut cfg = basic_config();
        cfg.remove("programs");
        let err = d
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .err()
            .expect("must error");
        assert!(err.to_string().contains("programs"), "{err}");
    }

    #[tokio::test]
    async fn parse_rejects_unknown_keys() {
        require_nix!();
        let d = driver();
        let mut cfg = basic_config();
        cfg.insert(
            "bogus".to_string(),
            TargetSpecValue::String("x".to_string()),
        );
        let err = d
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .err()
            .expect("must error");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[tokio::test]
    async fn parse_declares_nix_tool_input() {
        require_nix!();
        let d = driver();
        let res = d
            .parse(make_parse_req("//pkg:t", basic_config()), &ctoken())
            .await
            .unwrap();
        assert_eq!(res.target_def.inputs.len(), 1);
        let i = &res.target_def.inputs[0];
        assert!(matches!(i.mode, InputMode::Tool));
        assert_eq!(i.origin_id, "nix");
        assert_eq!(i.r#ref.r#ref.package.as_str(), "@heph/bin");
        assert_eq!(i.r#ref.r#ref.name, "nix");
    }

    #[tokio::test]
    async fn parse_outputs_one_group_per_program() {
        require_nix!();
        let d = driver();
        let mut cfg = basic_config();
        cfg.insert(
            "programs".to_string(),
            TargetSpecValue::List(vec![
                TargetSpecValue::String("rg".to_string()),
                TargetSpecValue::String("fd".to_string()),
            ]),
        );
        let res = d
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .unwrap();
        // One Output group per program — each with exactly 1 FilePath. The
        // engine's tool-input link check rejects multi-FilePath tool targets,
        // so consumers must pick a single program via `|<program>`.
        assert_eq!(res.target_def.outputs.len(), 2);
        let groups: Vec<&str> = res
            .target_def
            .outputs
            .iter()
            .map(|o| o.group.as_str())
            .collect();
        // Sorted (programs sorted at parse): fd before rg.
        assert_eq!(groups, vec!["fd", "rg"]);
        for output in &res.target_def.outputs {
            assert_eq!(
                output.paths.len(),
                1,
                "group `{}` must have exactly 1 FilePath",
                output.group
            );
            let p = &output.paths[0];
            match &p.content {
                Content::FilePath(s) => {
                    assert_eq!(s, &format!("pkg/bin/{}", output.group));
                }
                other => panic!("expected FilePath, got {other}"),
            }
            assert!(p.collect, "wrapper outputs must be collected by the bridge");
        }
    }

    #[tokio::test]
    async fn parse_sets_cache_flags() {
        require_nix!();
        let d = driver();
        let res = d
            .parse(make_parse_req("//pkg:t", basic_config()), &ctoken())
            .await
            .unwrap();
        assert!(res.target_def.cache);
        assert!(
            res.target_def.disable_remote_cache,
            "wrappers point at host-local /nix/store; remote cache must stay disabled"
        );
    }

    async fn hash_of(cfg: HashMap<String, TargetSpecValue>) -> Vec<u8> {
        driver()
            .parse(make_parse_req("//pkg:t", cfg), &ctoken())
            .await
            .unwrap()
            .target_def
            .hash
    }

    #[tokio::test]
    async fn hash_changes_with_nixpkgs() {
        require_nix!();
        let a = hash_of(basic_config()).await;
        let mut cfg = basic_config();
        cfg.insert(
            "nixpkgs".to_string(),
            TargetSpecValue::String("github:NixOS/nixpkgs/zzz".to_string()),
        );
        let b = hash_of(cfg).await;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn hash_changes_with_packages() {
        require_nix!();
        let a = hash_of(basic_config()).await;
        let mut cfg = basic_config();
        cfg.insert(
            "packages".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("pkgs.fd".to_string())]),
        );
        let b = hash_of(cfg).await;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn hash_changes_with_programs() {
        require_nix!();
        let a = hash_of(basic_config()).await;
        let mut cfg = basic_config();
        cfg.insert(
            "programs".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("fd".to_string())]),
        );
        let b = hash_of(cfg).await;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn hash_changes_with_system() {
        require_nix!();
        let a = hash_of(basic_config()).await;
        let mut cfg = basic_config();
        cfg.insert(
            "system".to_string(),
            TargetSpecValue::String("riscv64-linux".to_string()),
        );
        let b = hash_of(cfg).await;
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn explicit_system_differs_from_host_default() {
        require_nix!();
        let host_hash = hash_of(basic_config()).await;
        // Pick a system the host is provably not running on.
        let other = if host_nix_system().unwrap() == "aarch64-darwin" {
            "x86_64-linux"
        } else {
            "aarch64-darwin"
        };
        let mut cfg = basic_config();
        cfg.insert(
            "system".to_string(),
            TargetSpecValue::String(other.to_string()),
        );
        let other_hash = hash_of(cfg).await;
        assert_ne!(
            host_hash, other_hash,
            "system MUST be in def.hash so x86 builds don't cache-hit arm entries"
        );
    }

    #[tokio::test]
    async fn hash_is_order_independent_for_packages_and_programs() {
        require_nix!();
        let mut cfg_a = basic_config();
        cfg_a.insert(
            "packages".to_string(),
            TargetSpecValue::List(vec![
                TargetSpecValue::String("pkgs.fd".to_string()),
                TargetSpecValue::String("pkgs.ripgrep".to_string()),
            ]),
        );
        cfg_a.insert(
            "programs".to_string(),
            TargetSpecValue::List(vec![
                TargetSpecValue::String("fd".to_string()),
                TargetSpecValue::String("rg".to_string()),
            ]),
        );

        let mut cfg_b = basic_config();
        cfg_b.insert(
            "packages".to_string(),
            TargetSpecValue::List(vec![
                TargetSpecValue::String("pkgs.ripgrep".to_string()),
                TargetSpecValue::String("pkgs.fd".to_string()),
            ]),
        );
        cfg_b.insert(
            "programs".to_string(),
            TargetSpecValue::List(vec![
                TargetSpecValue::String("rg".to_string()),
                TargetSpecValue::String("fd".to_string()),
            ]),
        );

        let a = hash_of(cfg_a).await;
        let b = hash_of(cfg_b).await;
        assert_eq!(a, b);
    }

    #[test]
    fn nix_expr_contains_packages_and_system() {
        require_nix!();
        let locked = "github:NixOS/nixpkgs/abc1234?narHash=sha256-xxx";
        let packages = vec!["pkgs.ripgrep".to_string(), "pkgs.fd".to_string()];
        let expr = build_nix_expr(locked, "x86_64-linux", &packages, "rheph-test");
        assert!(expr.contains(locked), "{expr}");
        assert!(expr.contains("legacyPackages.\"x86_64-linux\""), "{expr}");
        assert!(expr.contains("pkgs.ripgrep"), "{expr}");
        assert!(expr.contains("pkgs.fd"), "{expr}");
        assert!(expr.contains("buildEnv"), "{expr}");
    }

    #[test]
    fn sanitize_env_name_replaces_unsafe_chars() {
        require_nix!();
        assert_eq!(sanitize_env_name("//pkg/sub:tool"), "rheph-__pkg_sub_tool");
        assert_eq!(sanitize_env_name("a-b_c.d"), "rheph-a-b_c.d");
    }
}
