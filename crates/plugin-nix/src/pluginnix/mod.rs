use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hdriver_support::run_log::{RunLog, Streams};
use hexecrunner::RunnerRef;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as TPath};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, HostPath,
    ParseRequest, ParseResponse, TargetAddr,
};
use hplugin::error::ProcessFailed;
use hplugin::htspec::Spec;
use hproc::proc_exec;
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
///
/// v3: the run declares the env's store path as a host path, so the engine
/// rebuilds an entry whose store path is gone instead of serving it; and each
/// wrapper checks its program exists before exec'ing it, exiting 127 with a
/// `heph run --force` hint when it does not. A v2 entry declares nothing and
/// would never be checked, so it is rebuilt once.
const DRIVER_FORMAT_VERSION: u32 = 3;

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

/// Config for a `nix` target. `#[derive(Spec)]` provides the parser and the LSP
/// schema. `packages`/`programs` presence is enforced by `required`; their
/// non-emptiness is checked in `parse` (a present empty list is still invalid).
#[derive(Spec)]
struct NixSpec {
    /// nixpkgs reference (required).
    #[spec(required)]
    nixpkgs: String,
    /// Nix packages to provide (required, non-empty).
    #[spec(required)]
    packages: Vec<String>,
    /// Programs to expose from the packages (required, non-empty).
    #[spec(required)]
    programs: Vec<String>,
    /// Nix system (e.g. `aarch64-darwin`); defaults to the host.
    #[spec(ty = ParamType::String)]
    system: Option<String>,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. Defaults to 1 cpu. Never hashed.
    resources: hplugin::driver::Resources,
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

/// Run one `nix` invocation and return its output whole.
///
/// Under `log_capture` (`log` is `Some`) nix's stderr — fetching, evaluation
/// and build progress — reaches the run's log as nix writes it. Its stdout is
/// nix's answer (flake metadata, a store path): data, never the log, on either
/// host. An older host keeps no log, as before: the output is captured whole.
async fn run_nix(
    spec: proc_exec::Spec,
    log: Option<&mut RunLog<'_>>,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<std::process::Output> {
    match log {
        Some(log) => {
            log.output(RunnerRef::local(), spec, Streams::STDERR, ctoken)
                .await
        }
        None => hexecrunner::output(RunnerRef::local(), spec, ctoken).await,
    }
}

/// A nix invocation that exited non-zero. Under `log_capture` its stderr is
/// already in the run's log, so the error is the exit status with no path; an
/// older host gets nix's stderr in the message, as before.
fn nix_failed(output: &std::process::Output, captured: bool, what: String) -> anyhow::Error {
    if captured {
        return anyhow::Error::new(ProcessFailed::from_status(output.status)).context(what);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::anyhow!("{what}: {stderr}")
}

/// A flake ref resolved by `nix flake metadata`.
#[derive(Debug)]
struct LockedFlake {
    /// The immutable URL, rev / narHash included.
    url: String,
    /// The ref names something on this host (a `path:` or `git+file:` flake),
    /// whose content can change under the same URL: never recorded (O8).
    local: bool,
}

/// Resolve a (possibly mutable) flake URL to its locked form via
/// `nix flake metadata --json`. Output `.url` is the immutable URL containing
/// the resolved rev / narHash, suitable for `getFlake` in pure-eval mode.
async fn lock_flake_url(
    nix_bin: &str,
    url: &str,
    log: Option<&mut RunLog<'_>>,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<LockedFlake> {
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

    let captured = log.is_some();
    let output = run_nix(spec, log, ctoken)
        .await
        .context("wait for nix flake metadata")?;
    if !output.status.success() {
        return Err(nix_failed(
            &output,
            captured,
            format!("nix flake metadata failed for {url}"),
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse nix flake metadata json")?;
    let locked = v
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("nix flake metadata json missing `url` field"))?;
    let path_type = v
        .get("locked")
        .and_then(|l| l.get("type"))
        .and_then(|t| t.as_str())
        == Some("path");
    Ok(LockedFlake {
        url: locked.to_string(),
        local: path_type || is_local_url(locked),
    })
}

/// A flake URL whose scheme reads from this host's filesystem: `path:`,
/// `file:`, or any `<fetcher>+file:` (`git+file:`, `tarball+file:`, …). A URL
/// with no scheme cannot be told apart, so it counts as local too.
fn is_local_url(url: &str) -> bool {
    match url.split_once(':') {
        Some((scheme, _)) => scheme == "path" || scheme == "file" || scheme.ends_with("+file"),
        None => true,
    }
}

// The nixpkgs lock record (I9, Decision 10).
//
// A target resolves its nixpkgs ref once per def hash: the first successful
// run writes `<nix home>/locks/<addr hash>/<def hash>`, and every later run of
// that def builds the recorded URL without calling `nix flake metadata`. A
// stale-cache re-run then rebuilds the same input-addressed store path, so the
// wrappers' bytes, and every dependent's key, stay put. Only a forced run
// (`RunRequest::force`) resolves again and replaces it.
//
// The record is two lines, the ref and the locked URL. It carries no version
// tag: any change to what a record means bumps `DRIVER_FORMAT_VERSION`, which
// moves the def hash and so the record's name. It never reaches a hash (I1).

/// The most bytes a record may hold; a bigger one is not ours.
const MAX_LOCK_RECORD: u64 = 16 * 1024;

/// Where `def`'s lock record lives under the driver's home.
fn lock_record_path(nix_home: &std::path::Path, def: &TargetDef) -> anyhow::Result<PathBuf> {
    // `parse` makes the def hash a lowercase hex string; anything else would
    // not be a safe file name.
    let def_hash = std::str::from_utf8(&def.hash)
        .ok()
        .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "nix driver: def hash of {} is not a file name: {:?}",
                def.addr.format(),
                def.hash
            )
        })?;
    Ok(nix_home
        .join("locks")
        .join(def.addr.hash_str())
        .join(def_hash))
}

/// Why `url` cannot be built from a record, if it cannot. It is spliced into a
/// nix string literal and passed as one argument, so it must be one plain
/// token; and a local flake is never recorded (O8).
fn locked_url_problem(url: &str) -> Option<&'static str> {
    if url.is_empty() {
        Some("the locked URL is empty")
    } else if url.starts_with('-') {
        Some("the locked URL starts with '-'")
    } else if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("the locked URL holds whitespace or a control character")
    } else if url.contains(['"', '\\', '$']) {
        Some("the locked URL holds a character a nix string cannot carry verbatim")
    } else if is_local_url(url) {
        Some("the locked URL names a local flake")
    } else {
        None
    }
}

/// The locked URL a record holds for `nixpkgs`, or why it holds none.
fn parse_lock_record(bytes: &[u8], nixpkgs: &str) -> Result<String, &'static str> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err("the record is not UTF-8");
    };
    let body = text
        .strip_suffix('\n')
        .ok_or("the record does not end in a newline")?;
    let (recorded_ref, url) = body.split_once('\n').ok_or("the record is not two lines")?;
    if url.contains('\n') {
        return Err("the record is more than two lines");
    }
    if recorded_ref != nixpkgs {
        return Err("the record is for another ref");
    }
    match locked_url_problem(url) {
        Some(problem) => Err(problem),
        None => Ok(url.to_owned()),
    }
}

/// What a read of a lock record found.
#[derive(Debug, PartialEq, Eq)]
enum Recorded {
    Absent,
    Valid(String),
    /// Present, but not a record this driver would have written: treated as
    /// absent, and overwritten by the run.
    Invalid(String),
}

/// Read the lock record at `path` for `nixpkgs`. Blocking.
fn read_lock_record(path: &std::path::Path, nixpkgs: &str) -> Recorded {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Recorded::Absent,
        Err(e) => return Recorded::Invalid(format!("opening it failed: {e}")),
    };
    let mut bytes = Vec::new();
    if let Err(e) = file.take(MAX_LOCK_RECORD + 1).read_to_end(&mut bytes) {
        return Recorded::Invalid(format!("reading it failed: {e}"));
    }
    if bytes.len() as u64 > MAX_LOCK_RECORD {
        return Recorded::Invalid(format!("it is over {MAX_LOCK_RECORD} bytes"));
    }
    match parse_lock_record(&bytes, nixpkgs) {
        Ok(url) => Recorded::Valid(url),
        Err(reason) => Recorded::Invalid(reason.to_owned()),
    }
}

/// Why a fresh resolution of `nixpkgs` is not recorded, if it is not. The
/// build still uses it; the next run resolves again, as before records.
fn unrecordable(nixpkgs: &str, locked: &LockedFlake) -> Option<&'static str> {
    if locked.local {
        Some("the ref is a local flake")
    } else if nixpkgs.contains(['\n', '\r']) {
        Some("the ref holds a line break")
    } else {
        locked_url_problem(&locked.url)
    }
}

/// Write `nixpkgs`'s lock record at `path`: a temp file in the same directory,
/// renamed into place, so a reader sees a whole record or none and two writers
/// never share a temp name. The last rename wins. Blocking.
fn write_lock_record(path: &std::path::Path, nixpkgs: &str, url: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("lock record {} has no parent", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // A directory where the record goes is a bad record; the rename below
    // cannot replace it.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("remove the directory at {}", path.display()))?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create a temp file in {}", dir.display()))?;
    tmp.write_all(format!("{nixpkgs}\n{url}\n").as_bytes())
        .with_context(|| format!("write {}", tmp.path().display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("rename the record into {}", path.display()))?;
    Ok(())
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
    out.push_str("heph-");
    for c in addr_str.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// The one store path `nix build --print-out-paths` printed. `buildEnv` has a
/// single output, so anything but one non-empty line is refused rather than
/// guessed at.
fn parse_out_path(stdout: &[u8]) -> anyhow::Result<&str> {
    let text = std::str::from_utf8(stdout).context("nix build stdout not utf-8")?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let path = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("nix build produced empty store path"))?;
    anyhow::ensure!(
        lines.next().is_none(),
        "nix build printed more than one store path: {text:?}"
    );
    Ok(path)
}

/// Quote `s` as one POSIX shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The wrapper for one program. It checks the program is still there before
/// exec'ing it: a store path collected after the entry was cached would
/// otherwise fail as a bare "not found", with nothing pointing at the fix.
fn wrapper_script(target_bin: &str, addr: &str) -> String {
    let bin = sh_quote(target_bin);
    let missing = sh_quote(&format!("{target_bin} missing; heph run --force {addr}"));
    format!(
        "#!/bin/sh\n\
         [ -e {bin} ] || {{ printf '%s\\n' {missing} >&2; exit 127; }}\n\
         exec {bin} \"$@\"\n"
    )
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        NixSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = NixSpec::from(&req.target_spec.config)?;
        let mut packages = spec.packages;
        let mut programs = spec.programs;
        let system = match spec.system {
            Some(s) => s,
            None => host_nix_system()?,
        };

        if packages.is_empty() {
            anyhow::bail!("nix driver requires `packages` (non-empty list)");
        }
        if programs.is_empty() {
            anyhow::bail!("nix driver requires `programs` (non-empty list)");
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
            hashed: true,
            runtime: true,
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
            nixpkgs: spec.nixpkgs,
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
                // The wrapper points at host-local /nix/store paths; remote
                // consumers would not have those paths realized.
                cache: CacheConfig::on(false),
                hash,
                resources: spec.resources,
                ..Default::default()
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
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def::<NixDef>();
        let addr_str = req.request.target.addr.format();
        // Under `log_capture` the request's writers are the run's log. An older
        // host gets no log from this driver, as before.
        let mut log = req
            .request
            .log_capture
            .then(|| RunLog::from_request(&mut req.request, "log.txt"));

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
        //
        // A def resolves once: a recorded lock is built as is, and only a
        // forced run resolves again (I9).
        let target = &req.request.target;
        let record_path = lock_record_path(&self.home_dir, target)?;
        let recorded = if req.request.force {
            None
        } else {
            let (path, nixpkgs) = (record_path.clone(), def.nixpkgs.clone());
            match hcore::blocking::run(move || read_lock_record(&path, &nixpkgs)).await {
                Recorded::Absent => None,
                Recorded::Valid(url) => Some(url),
                Recorded::Invalid(reason) => {
                    tracing::warn!(
                        addr = %addr_str,
                        record = %record_path.display(),
                        %reason,
                        "ignoring the recorded nixpkgs lock; resolving {} again",
                        def.nixpkgs
                    );
                    None
                }
            }
        };
        let from_record = recorded.is_some();
        let (locked_flake_url, record) = match recorded {
            Some(url) => {
                tracing::info!(addr = %addr_str, url = %url, "building from recorded lock {url}");
                (url, false)
            }
            None => {
                let locked = lock_flake_url(&nix_bin, &def.nixpkgs, log.as_mut(), ctoken)
                    .await
                    .with_context(|| format!("lock flake url {}", def.nixpkgs))?;
                let record = match unrecordable(&def.nixpkgs, &locked) {
                    None => true,
                    Some(reason) => {
                        tracing::debug!(
                            addr = %addr_str,
                            url = %locked.url,
                            reason,
                            "not recording the nixpkgs lock"
                        );
                        false
                    }
                };
                (locked.url, record)
            }
        };

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

        let output = run_nix(spec, log.as_mut(), ctoken)
            .await
            .with_context(|| format!("wait for nix build for {addr_str}"))?;

        if !output.status.success() {
            // A recorded lock is never re-resolved behind the user's back: a
            // fallback would move the store path, and every dependent's key,
            // without anyone asking for it.
            let what = if from_record {
                format!(
                    "nix build failed for {addr_str} from its recorded lock {locked_flake_url}; \
                     `heph run --force {addr_str}` resolves {} again",
                    def.nixpkgs
                )
            } else {
                format!("nix build failed for {addr_str}")
            };
            return Err(nix_failed(&output, log.is_some(), what));
        }

        let store_path = parse_out_path(&output.stdout)?;
        // The env's store path is the one host path the outputs depend on:
        // every wrapper execs under it, and its closure holds everything the
        // programs reference, which nix never collects while it is live (I2).
        let env_path = HostPath::new(store_path).with_context(|| {
            format!("nix build for {addr_str} printed store path {store_path:?}")
        })?;

        // Write one wrapper per program into the sandbox so the bridge can
        // collect them into the output tar.
        let bin_dir = req.sandbox_pkg_dir.join("bin");
        tokio::fs::create_dir_all(&bin_dir)
            .await
            .with_context(|| format!("create {:?}", bin_dir))?;

        for program in &def.programs {
            let target_bin = format!("{}/bin/{program}", env_path.as_str());
            tokio::fs::metadata(&target_bin).await.with_context(|| {
                format!("program `{program}` not present at {target_bin} after nix build")
            })?;
            let wrapper = wrapper_script(&target_bin, &addr_str);
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

        // Last, once everything else succeeded: a failed or cancelled run
        // never leaves a record, and a failed write fails the run (I9).
        if record {
            anyhow::ensure!(
                !ctoken.is_cancelled(),
                "nix driver: {addr_str} was cancelled before its nixpkgs lock was recorded"
            );
            let (path, nixpkgs, url) = (
                record_path.clone(),
                def.nixpkgs.clone(),
                locked_flake_url.clone(),
            );
            hcore::blocking::run(move || write_lock_record(&path, &nixpkgs, &url))
                .await
                .with_context(|| {
                    format!(
                        "record the nixpkgs lock of {addr_str} at {}",
                        record_path.display()
                    )
                })?;
        }

        // The bridge appends the collected tar artifact based on `outputs[]`
        // with `collect: true`. Nothing to add here.
        Ok(ManagedRunResponse {
            host_paths: vec![env_path],
            artifacts: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
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

    fn make_parse_req(addr: &str, config: HashMap<String, Value>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr(addr).unwrap(),
                driver: DRIVER_NAME.to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    fn basic_config() -> HashMap<String, Value> {
        HashMap::from([
            (
                "nixpkgs".to_string(),
                Value::String("github:NixOS/nixpkgs/abc".to_string()),
            ),
            (
                "packages".to_string(),
                Value::List(vec![Value::String("pkgs.ripgrep".to_string())]),
            ),
            (
                "programs".to_string(),
                Value::List(vec![Value::String("rg".to_string())]),
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
        cfg.insert("bogus".to_string(), Value::String("x".to_string()));
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
            Value::List(vec![
                Value::String("rg".to_string()),
                Value::String("fd".to_string()),
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
        assert!(res.target_def.cache.enabled);
        assert!(
            !res.target_def.cache.remote_enabled,
            "wrappers point at host-local /nix/store; remote cache must stay disabled"
        );
    }

    async fn hash_of(cfg: HashMap<String, Value>) -> Vec<u8> {
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
            Value::String("github:NixOS/nixpkgs/zzz".to_string()),
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
            Value::List(vec![Value::String("pkgs.fd".to_string())]),
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
            Value::List(vec![Value::String("fd".to_string())]),
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
            Value::String("riscv64-linux".to_string()),
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
        cfg.insert("system".to_string(), Value::String(other.to_string()));
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
            Value::List(vec![
                Value::String("pkgs.fd".to_string()),
                Value::String("pkgs.ripgrep".to_string()),
            ]),
        );
        cfg_a.insert(
            "programs".to_string(),
            Value::List(vec![
                Value::String("fd".to_string()),
                Value::String("rg".to_string()),
            ]),
        );

        let mut cfg_b = basic_config();
        cfg_b.insert(
            "packages".to_string(),
            Value::List(vec![
                Value::String("pkgs.ripgrep".to_string()),
                Value::String("pkgs.fd".to_string()),
            ]),
        );
        cfg_b.insert(
            "programs".to_string(),
            Value::List(vec![
                Value::String("rg".to_string()),
                Value::String("fd".to_string()),
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
        let expr = build_nix_expr(locked, "x86_64-linux", &packages, "heph-test");
        assert!(expr.contains(locked), "{expr}");
        assert!(expr.contains("legacyPackages.\"x86_64-linux\""), "{expr}");
        assert!(expr.contains("pkgs.ripgrep"), "{expr}");
        assert!(expr.contains("pkgs.fd"), "{expr}");
        assert!(expr.contains("buildEnv"), "{expr}");
    }

    #[test]
    fn sanitize_env_name_replaces_unsafe_chars() {
        require_nix!();
        assert_eq!(sanitize_env_name("//pkg/sub:tool"), "heph-__pkg_sub_tool");
        assert_eq!(sanitize_env_name("a-b_c.d"), "heph-a-b_c.d");
    }
}

/// A stand-in `nix` for the run tests: a shell script handed to the driver as
/// its staged `nix` tool, plus a fake store path holding `rg` and `fd`. Its
/// behaviour is read from control files at each call, so one test can change
/// it between runs, and it appends each call's arguments to `calls`.
#[cfg(test)]
mod fake {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hdriver_support::driver_managed::ManagedRunInput;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::{RunInput, RunRequest, inputartifact, outputartifact};
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    pub(super) const STORE_TOKEN: &str = "STORE-PATH-MARKER";
    pub(super) const REF: &str = "github:o/r/main";
    pub(super) const LOCKED: &str = "github:o/r/STORE-PATH-MARKER-locked";

    pub(super) struct Fake {
        pub(super) dir: tempfile::TempDir,
        pub(super) nix: PathBuf,
        pub(super) store: PathBuf,
    }

    impl Fake {
        /// `flake metadata` prints a progress line, then `{"url": LOCKED}`.
        /// The build prints a progress line, then the store path.
        pub(super) fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = dir.path().join(format!("{STORE_TOKEN}-env"));
            std::fs::create_dir_all(store.join("bin")).expect("mkdir store");
            for p in ["rg", "fd"] {
                std::fs::write(store.join("bin").join(p), b"").expect("write program");
            }
            let d = sh_quote(&dir.path().display().to_string());
            let s = sh_quote(&store.display().to_string());
            let nix = dir.path().join("nix");
            std::fs::write(
                &nix,
                format!(
                    "#!/bin/sh\n\
                     d={d}\n\
                     printf '%s\\n' \"$*\" >> \"$d/calls\"\n\
                     case \"$*\" in\n\
                     *'flake metadata'*) echo 'fetching flake' >&2; cat \"$d/meta.json\" ;;\n\
                     *build*) echo 'building env' >&2\n\
                       if [ -e \"$d/build_sleep\" ]; then exec sleep 30; fi\n\
                       if [ -e \"$d/build_fail\" ]; then \
                         echo 'error: builder for env failed' >&2; exit 1; fi\n\
                       echo {s} ;;\n\
                     esac\n"
                ),
            )
            .expect("write nix");
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&nix, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let fake = Self { dir, nix, store };
            fake.set_meta(&format!("{{\"url\":\"{LOCKED}\"}}"));
            fake
        }

        fn flag(&self, name: &str, on: bool) {
            let p = self.dir.path().join(name);
            if on {
                std::fs::write(&p, b"").expect("set flag");
            } else if p.exists() {
                std::fs::remove_file(&p).expect("clear flag");
            }
        }

        /// The build prints an error line and exits 1.
        pub(super) fn fail_build(&self, on: bool) {
            self.flag("build_fail", on);
        }

        /// The build becomes a 30s sleep, so a test can cancel it mid-build;
        /// `exec`, so the kill reaches the sleep itself.
        pub(super) fn sleep_build(&self, on: bool) {
            self.flag("build_sleep", on);
        }

        /// What `nix flake metadata --json` prints.
        pub(super) fn set_meta(&self, json: &str) {
            std::fs::write(self.dir.path().join("meta.json"), json).expect("write meta");
        }

        fn calls(&self) -> Vec<String> {
            match std::fs::read_to_string(self.dir.path().join("calls")) {
                Ok(s) => s.lines().map(str::to_owned).collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => panic!("read calls: {e}"),
            }
        }

        pub(super) fn metadata_calls(&self) -> usize {
            self.calls()
                .iter()
                .filter(|c| c.contains("flake metadata"))
                .count()
        }

        /// Every `nix build` call's arguments, oldest first.
        pub(super) fn build_calls(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c.contains(" build "))
                .collect()
        }

        pub(super) fn sandbox(&self) -> PathBuf {
            self.dir.path().join("sandbox")
        }

        /// The driver's home, `<home>/nix-driver` in production.
        pub(super) fn home(&self) -> PathBuf {
            self.dir.path().join("home")
        }
    }

    pub(super) async fn target_def() -> TargetDef {
        target_def_with(&["rg"]).await
    }

    pub(super) async fn target_def_with(programs: &[&str]) -> TargetDef {
        let config = HashMap::from([
            ("nixpkgs".to_string(), Value::String(REF.to_string())),
            (
                "packages".to_string(),
                Value::List(vec![Value::String("pkgs.ripgrep".to_string())]),
            ),
            (
                "programs".to_string(),
                Value::List(
                    programs
                        .iter()
                        .map(|p| Value::String((*p).to_string()))
                        .collect(),
                ),
            ),
        ]);
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr("//pkg:t").expect("addr"),
                driver: DRIVER_NAME.to_string(),
                config,
                ..Default::default()
            }),
        };
        Driver::new(PathBuf::from("/nonexistent"))
            .parse(req, &StdCancellationToken::new())
            .await
            .expect("parse")
            .target_def
    }

    /// Run the driver against `fake`; returns the result and what reached the
    /// request's stdout and stderr writers.
    pub(super) async fn run(
        fake: &Fake,
        log_capture: bool,
    ) -> (anyhow::Result<ManagedRunResponse>, String, String) {
        let def = target_def().await;
        run_def(fake, &def, log_capture, false, &StdCancellationToken::new()).await
    }

    /// Run `def` against `fake` with the request's `force` bit and `ctoken`.
    pub(super) async fn run_def(
        fake: &Fake,
        def: &TargetDef,
        log_capture: bool,
        force: bool,
        ctoken: &StdCancellationToken,
    ) -> (anyhow::Result<ManagedRunResponse>, String, String) {
        let id = "test".to_string();
        let sandbox = fake.sandbox();
        let pkg = sandbox.join("ws/pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir pkg");
        let list = sandbox.join("nix.list");
        std::fs::write(&list, format!("{}\n", fake.nix.display())).expect("write list");
        let (mut out, mut err) = (Vec::<u8>::new(), Vec::<u8>::new());
        let mut request = RunRequest::new(&id, def, &sandbox);
        request.log_capture = log_capture;
        request.force = force;
        request.stdout = Some(&mut out);
        request.stderr = Some(&mut err);
        let req = ManagedRunRequest {
            request,
            sandbox_dir: sandbox.clone(),
            sandbox_ws_dir: sandbox.join("ws"),
            sandbox_pkg_dir: pkg,
            inputs: vec![ManagedRunInput {
                input: RunInput {
                    artifact: inputartifact::InputArtifact {
                        r#type: inputartifact::Type::Dep,
                        origin_id: "nix".to_string(),
                        content: Arc::new(outputartifact::OutputArtifact {
                            group: String::new(),
                            name: String::new(),
                            r#type: outputartifact::Type::Output,
                            content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                                data: vec![],
                                path: String::new(),
                                x: false,
                            }),
                            hashout: "test".to_string(),
                        }),
                    },
                    origin_id: "nix".to_string(),
                    source_addr: parse_addr("//@heph/bin:nix").expect("addr"),
                    filters: vec![],
                    annotations: std::collections::BTreeMap::new(),
                },
                list_path: Some(list),
                unpack_root: sandbox.join("ws"),
            }],
        };
        let res = Driver::new(fake.home()).run(req, ctoken).await;
        (
            res,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }
}

/// Where nix's output goes: its stderr is the run's log, its stdout (flake
/// metadata, a store path) is data and never the log.
#[cfg(test)]
mod log_tests {
    use super::fake::{Fake, STORE_TOKEN, run};
    use super::*;

    /// Under `log_capture` the flake lock's and the build's progress reach the
    /// log; the flake metadata and the store path (stdout) never do, and the
    /// driver still builds from them.
    #[tokio::test]
    async fn nix_progress_reaches_the_log_and_its_answers_never_do() {
        let fake = Fake::new();
        let (res, out, err) = run(&fake, true).await;
        let res = res.expect("run");
        assert!(res.artifacts.is_empty(), "no Log artifact crosses");
        assert_eq!(err, "fetching flake\nbuilding env\n");
        assert_eq!(out, "", "nix's stdout is data, never the log");
        assert!(!err.contains(STORE_TOKEN), "{err}");
        let wrapper = std::fs::read_to_string(fake.sandbox().join("ws/pkg/bin/rg"))
            .expect("the wrapper is built from the store path");
        assert!(
            wrapper.contains(&fake.store.display().to_string()),
            "{wrapper}"
        );
        assert!(!fake.sandbox().join("log.txt").exists());
    }

    /// A failed build under `log_capture`: nix's stderr is in the log, and the
    /// error is the exit status, naming no log path and not repeating it. An
    /// older host keeps today's error, with stderr in the message, and its
    /// writers see nothing.
    #[tokio::test]
    async fn a_failed_build_is_logged_under_log_capture_and_unchanged_without() {
        let fake = Fake::new();
        fake.fail_build(true);
        let (res, out, err) = run(&fake, true).await;
        let Err(e) = res else {
            panic!("the build fails")
        };
        assert!(
            e.chain()
                .any(|c| c.downcast_ref::<ProcessFailed>().is_some()),
            "{e:#}"
        );
        assert!(hplugin::error::failure_log(&e).is_none(), "{e:#}");
        let msg = format!("{e:#}");
        assert!(msg.contains("nix build failed for //pkg:t"), "{msg}");
        assert!(!msg.contains("builder for env failed"), "{msg}");
        assert_eq!(
            err,
            "fetching flake\nbuilding env\nerror: builder for env failed\n"
        );
        assert_eq!(out, "");

        let (res, out, err) = run(&fake, false).await;
        let Err(e) = res else {
            panic!("the build fails")
        };
        let msg = format!("{e:#}");
        assert!(
            msg.contains(
                "nix build failed for //pkg:t: building env\nerror: builder for env failed"
            ),
            "{msg}"
        );
        assert_eq!((out.as_str(), err.as_str()), ("", ""));
        assert!(!fake.sandbox().join("log.txt").exists());
    }
}

/// The env's store path is declared as the run's one host path, and every
/// wrapper depends on nothing outside it.
#[cfg(test)]
mod host_path_tests {
    use super::fake::{Fake, run, run_def, target_def_with};
    use super::*;
    use hcore::hasync::StdCancellationToken;

    /// The run response carries exactly the path `--print-out-paths` printed.
    #[tokio::test]
    async fn nix_declares_env_path() {
        let fake = Fake::new();
        let (res, _, _) = run(&fake, false).await;
        let res = res.expect("run");
        let store = fake.store.to_str().expect("utf-8 store path");
        assert_eq!(
            res.host_paths,
            vec![HostPath::new(store).expect("host path")]
        );
    }

    /// The program path each wrapper checks and execs.
    fn wrapper_target(wrapper: &str) -> &str {
        let exec = wrapper
            .lines()
            .find_map(|l| l.strip_prefix("exec '"))
            .unwrap_or_else(|| panic!("no exec line in {wrapper}"));
        exec.strip_suffix("' \"$@\"")
            .unwrap_or_else(|| panic!("unexpected exec line in {wrapper}"))
    }

    /// Every wrapper execs a program under the declared path, so the one path
    /// covers the whole output (I2).
    #[tokio::test]
    async fn nix_wrappers_all_under_declared_path() {
        let fake = Fake::new();
        let def = target_def_with(&["rg", "fd"]).await;
        let (res, _, _) = run_def(&fake, &def, false, false, &StdCancellationToken::new()).await;
        let res = res.expect("run");
        let [declared] = res.host_paths.as_slice() else {
            panic!("one host path, got {:?}", res.host_paths)
        };
        let under = format!("{}/", declared.as_str());
        for program in ["fd", "rg"] {
            let wrapper = std::fs::read_to_string(fake.sandbox().join("ws/pkg/bin").join(program))
                .expect("wrapper");
            let target = wrapper_target(&wrapper);
            assert!(target.starts_with(&under), "{target} not under {under}");
            assert_eq!(target, format!("{under}bin/{program}"));
        }
    }

    /// A wrapper whose program is gone exits 127, naming the path and the
    /// command that rebuilds it; with the program present it execs it.
    #[tokio::test]
    async fn wrapper_checks_its_program_before_exec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let program = dir.path().join("it's here");
        std::fs::write(&program, "#!/bin/sh\necho ran \"$@\"\n").expect("write program");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let program = program.to_str().expect("utf-8");
        let wrapper = dir.path().join("wrapper");
        std::fs::write(&wrapper, wrapper_script(program, "//pkg:t")).expect("write wrapper");

        let out = std::process::Command::new("/bin/sh")
            .arg(&wrapper)
            .arg("a b")
            .output()
            .expect("run wrapper");
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ran a b\n");

        std::fs::remove_file(program).expect("remove program");
        let out = std::process::Command::new("/bin/sh")
            .arg(&wrapper)
            .output()
            .expect("run wrapper");
        assert_eq!(out.status.code(), Some(127), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("{program} missing; heph run --force //pkg:t\n")
        );
    }
}

/// The nixpkgs lock record (I9): a def resolves once, every later run builds
/// what it recorded, and only a forced run resolves again.
#[cfg(test)]
mod lock_record_tests {
    use super::fake::{Fake, LOCKED, REF, run_def, target_def, target_def_with};
    use super::*;
    use hcore::hasync::StdCancellationToken;

    const RECORDED: &str = "github:o/r/recorded-rev";

    fn record_of(fake: &Fake, def: &TargetDef) -> PathBuf {
        lock_record_path(&fake.home(), def).expect("record path")
    }

    fn plant(path: &std::path::Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("plant record");
    }

    fn read(path: &std::path::Path) -> Option<String> {
        match std::fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("read {}: {e}", path.display()),
        }
    }

    fn record_text(url: &str) -> String {
        format!("{REF}\n{url}\n")
    }

    async fn run(fake: &Fake, def: &TargetDef, force: bool) -> anyhow::Result<()> {
        run_def(fake, def, false, force, &StdCancellationToken::new())
            .await
            .0
            .map(drop)
    }

    /// The record sits at `<nix home>/locks/<addr hash>/<def hash>`.
    #[tokio::test]
    async fn lock_record_path_is_keyed_by_addr_and_def_hash() {
        let def = target_def().await;
        let path = lock_record_path(std::path::Path::new("/h/nix-driver"), &def).expect("path");
        let def_hash = std::str::from_utf8(&def.hash).expect("hex");
        assert_eq!(
            path,
            PathBuf::from(format!(
                "/h/nix-driver/locks/{}/{def_hash}",
                def.addr.hash_str()
            ))
        );
    }

    /// With a record present, the run builds the recorded URL and never calls
    /// `nix flake metadata`.
    #[tokio::test]
    async fn nix_reuses_recorded_lock() {
        let fake = Fake::new();
        let def = target_def().await;
        let path = record_of(&fake, &def);
        plant(&path, record_text(RECORDED).as_bytes());

        run(&fake, &def, false).await.expect("run");
        assert_eq!(fake.metadata_calls(), 0);
        let builds = fake.build_calls();
        assert_eq!(builds.len(), 1, "{builds:?}");
        assert!(builds[0].contains(RECORDED), "{}", builds[0]);
        assert!(!builds[0].contains(LOCKED), "{}", builds[0]);
        assert_eq!(read(&path), Some(record_text(RECORDED)), "kept as is");
    }

    /// Without a record, the run resolves, builds, then records; the next run
    /// reuses it. A failed build writes none.
    #[tokio::test]
    async fn nix_records_lock_after_build() {
        let fake = Fake::new();
        let def = target_def().await;
        let path = record_of(&fake, &def);

        fake.fail_build(true);
        run(&fake, &def, false).await.expect_err("the build fails");
        assert_eq!(read(&path), None, "a failed build records nothing");

        fake.fail_build(false);
        run(&fake, &def, false).await.expect("run");
        assert_eq!(fake.metadata_calls(), 2);
        assert_eq!(read(&path), Some(record_text(LOCKED)));

        fake.set_meta(r#"{"url":"github:o/r/moved"}"#);
        run(&fake, &def, false).await.expect("run");
        assert_eq!(fake.metadata_calls(), 2, "the record is reused");
        let builds = fake.build_calls();
        assert!(builds[2].contains(LOCKED), "{}", builds[2]);
    }

    /// A forced run ignores the record, resolves again and replaces it.
    #[tokio::test]
    async fn nix_force_reresolves() {
        let fake = Fake::new();
        let def = target_def().await;
        let path = record_of(&fake, &def);
        plant(&path, record_text(RECORDED).as_bytes());

        run(&fake, &def, true).await.expect("forced run");
        assert_eq!(fake.metadata_calls(), 1);
        let builds = fake.build_calls();
        assert!(builds[0].contains(LOCKED), "{}", builds[0]);
        assert!(!builds[0].contains(RECORDED), "{}", builds[0]);
        assert_eq!(read(&path), Some(record_text(LOCKED)));
    }

    /// Each bad record counts as absent: the run resolves and overwrites it.
    #[tokio::test]
    async fn nix_bad_record_handled() {
        let cases: &[(&str, Vec<u8>)] = &[
            ("empty", b"".to_vec()),
            ("whitespace only", b"  \n \n".to_vec()),
            ("quote", format!("{REF}\ngithub:o/r/a\"b\n").into_bytes()),
            (
                "embedded newline",
                format!("{REF}\ngithub:o/r/a\nb\n").into_bytes(),
            ),
            (
                "non-UTF-8",
                [REF.as_bytes(), b"\ngithub:o/r/\xff\xfe\n"].concat(),
            ),
            (
                "leading '-'",
                format!("{REF}\n-github:o/r/a\n").into_bytes(),
            ),
            (
                "another ref",
                b"github:o/other/main\ngithub:o/other/rev\n".to_vec(),
            ),
            (
                "no trailing newline",
                format!("{REF}\n{RECORDED}").into_bytes(),
            ),
            (
                "local URL",
                format!("{REF}\npath:/x?narHash=y\n").into_bytes(),
            ),
            ("too big", vec![b'a'; MAX_LOCK_RECORD as usize + 1]),
        ];
        let def = target_def().await;
        for (name, bytes) in cases {
            assert!(
                parse_lock_record(bytes, REF).is_err(),
                "{name}: parses as a record"
            );
            let fake = Fake::new();
            let path = record_of(&fake, &def);
            plant(&path, bytes);
            assert!(
                matches!(read_lock_record(&path, REF), Recorded::Invalid(_)),
                "{name}"
            );
            run(&fake, &def, false)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(fake.metadata_calls(), 1, "{name}: resolves");
            assert_eq!(read(&path), Some(record_text(LOCKED)), "{name}");
        }

        // A directory in its place.
        let fake = Fake::new();
        let path = record_of(&fake, &def);
        std::fs::create_dir_all(path.join("sub")).expect("mkdir");
        assert!(matches!(read_lock_record(&path, REF), Recorded::Invalid(_)));
        run(&fake, &def, false).await.expect("run");
        assert_eq!(fake.metadata_calls(), 1);
        assert_eq!(read(&path), Some(record_text(LOCKED)));
    }

    /// The recorded URL fails to build: the run fails naming the command that
    /// resolves again, never calls `nix flake metadata`, and keeps the record.
    #[tokio::test]
    async fn nix_recorded_url_unfetchable_fails_with_force_hint() {
        let def = target_def().await;
        for log_capture in [false, true] {
            let fake = Fake::new();
            let path = record_of(&fake, &def);
            plant(&path, record_text(RECORDED).as_bytes());
            fake.fail_build(true);
            let (res, _, _) = run_def(
                &fake,
                &def,
                log_capture,
                false,
                &StdCancellationToken::new(),
            )
            .await;
            let msg = format!("{:#}", res.err().expect("the build fails"));
            assert!(msg.contains("heph run --force //pkg:t"), "{msg}");
            assert!(msg.contains(RECORDED), "{msg}");
            assert_eq!(fake.metadata_calls(), 0, "no fallback to resolving");
            assert_eq!(fake.build_calls().len(), 1, "no second build");
            assert_eq!(read(&path), Some(record_text(RECORDED)));
        }
    }

    /// A run that fails after `nix build` (a program the env lacks) writes no
    /// record; forced over an existing one, it keeps the old record.
    #[tokio::test]
    async fn nix_missing_program_writes_no_record() {
        let fake = Fake::new();
        let def = target_def_with(&["rg", "nope"]).await;
        let path = record_of(&fake, &def);
        let e = run(&fake, &def, false).await.expect_err("no `nope`");
        assert!(
            format!("{e:#}").contains("program `nope` not present"),
            "{e:#}"
        );
        assert_eq!(fake.metadata_calls(), 1);
        assert_eq!(read(&path), None);

        plant(&path, record_text(RECORDED).as_bytes());
        run(&fake, &def, true).await.expect_err("no `nope`");
        assert_eq!(fake.metadata_calls(), 2);
        assert_eq!(read(&path), Some(record_text(RECORDED)));
    }

    /// A run cancelled mid-build writes no record, and a forced one cancelled
    /// mid-build keeps the old record.
    #[tokio::test]
    async fn nix_cancelled_run_writes_no_record() {
        let def = target_def().await;
        for force in [false, true] {
            let fake = Fake::new();
            let path = record_of(&fake, &def);
            let old = force.then(|| record_text(RECORDED));
            if let Some(old) = &old {
                plant(&path, old.as_bytes());
            }
            fake.sleep_build(true);
            let ctoken = StdCancellationToken::new();
            // Cancel once the build has started (the fake logs its call
            // before sleeping).
            let cancel = {
                let ctoken = ctoken.clone();
                let calls = fake.dir.path().join("calls");
                tokio::spawn(async move {
                    for _ in 0..1000 {
                        let started = std::fs::read_to_string(&calls)
                            .is_ok_and(|c| c.lines().any(|l| l.contains(" build ")));
                        if started {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    ctoken.cancel();
                })
            };
            let (res, _, _) = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                run_def(&fake, &def, false, force, &ctoken),
            )
            .await
            .expect("the cancel stops the build");
            cancel.await.expect("cancel task");
            assert!(res.is_err(), "a cancelled run fails");
            assert_eq!(fake.build_calls().len(), 1);
            assert_eq!(read(&path), old, "force={force}");
        }
    }

    /// A forced run whose build fails keeps the old record.
    #[tokio::test]
    async fn nix_forced_failed_build_keeps_old_record() {
        let fake = Fake::new();
        let def = target_def().await;
        let path = record_of(&fake, &def);
        plant(&path, record_text(RECORDED).as_bytes());
        fake.fail_build(true);
        run(&fake, &def, true).await.expect_err("the build fails");
        assert_eq!(fake.metadata_calls(), 1, "forced runs resolve");
        assert_eq!(read(&path), Some(record_text(RECORDED)));
    }

    /// N concurrent writers leave exactly one of their values, whole, and no
    /// temp file.
    #[test]
    fn nix_concurrent_runs_record_one_whole_value() {
        const N: usize = 16;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("locks/addr/def");
        // Long values, so a torn write could not pass for a whole one.
        let urls: Vec<String> = (0..N)
            .map(|i| format!("github:o/r/{}", i.to_string().repeat(512)))
            .collect();
        let barrier = std::sync::Barrier::new(N);
        std::thread::scope(|s| {
            for url in &urls {
                let (path, barrier) = (&path, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    write_lock_record(path, REF, url).expect("write record");
                });
            }
        });
        let Recorded::Valid(url) = read_lock_record(&path, REF) else {
            panic!("not a whole record: {:?}", std::fs::read(&path))
        };
        assert!(urls.contains(&url), "{url}");
        let entries: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("def")]);
    }

    /// A ref whose metadata reports a path or git+file flake is never recorded,
    /// so the next run resolves again (O8).
    #[tokio::test]
    async fn nix_local_ref_not_recorded() {
        let def = target_def().await;
        for meta in [
            r#"{"url":"path:/src/nixpkgs?narHash=sha256-a","locked":{"type":"path","path":"/src/nixpkgs"}}"#,
            r#"{"url":"git+file:///src/nixpkgs?rev=abc","locked":{"type":"git","url":"file:///src/nixpkgs"}}"#,
        ] {
            let fake = Fake::new();
            fake.set_meta(meta);
            let path = record_of(&fake, &def);
            run(&fake, &def, false).await.expect("run");
            assert_eq!(read(&path), None, "{meta}");
            run(&fake, &def, false).await.expect("run");
            assert_eq!(fake.metadata_calls(), 2, "{meta}: resolves every run");
        }
    }

    #[test]
    fn local_urls() {
        for url in [
            "path:/x",
            "git+file:///x",
            "tarball+file:///x.tgz",
            "file:///x",
            "/x",
        ] {
            assert!(is_local_url(url), "{url}");
        }
        for url in ["github:o/r/rev", "git+https://x/y?rev=a", "https://x/y.tgz"] {
            assert!(!is_local_url(url), "{url}");
        }
    }
}
