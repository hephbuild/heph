//! The `http_fetch` driver: downloads a URL into a cacheable file output.
//!
//! One target = one fetched file. The URL is a **template over the target's addr
//! args**, so a single target definition serves every platform (or version, or
//! any other axis) a caller asks for:
//!
//! ```text
//! //@heph/go/govet/v1.2.3:heph-govet@goos=darwin,goarch=arm64
//!   url = "https://…/v1.2.3/heph-govet_{goos}_{goarch}"
//!       → https://…/v1.2.3/heph-govet_darwin_arm64
//! ```
//!
//! The rendered URL — not the template — is what the target hashes on, so each
//! arg combination is its own cache entry, and an unresolvable placeholder is an
//! error rather than a silently mis-fetched file.
//!
//! Fetching is a side-effect-free, content-addressed step like any other target:
//! `sha256` (when set) is verified before the file is written, so a changed
//! remote asset fails the build closed instead of poisoning the cache.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse,
};
use hplugin::htspec::{Spec, TargetSpecCache};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

pub const DRIVER_NAME: &str = "http_fetch";

/// Config for an `http_fetch` target.
#[derive(Spec)]
struct HttpFetchSpec {
    /// URL to download. `{arg}` placeholders are substituted with the target
    /// addr's args (e.g. `…/heph-govet_{goos}_{goarch}` on
    /// `:heph-govet@goos=linux,goarch=amd64`); `{{` / `}}` are literal braces.
    #[spec(required)]
    url: String,
    /// Expected SHA-256 of the downloaded bytes (hex). Omitted → the file is
    /// fetched **unverified** (the driver warns): the target is then only as
    /// reproducible as the remote server.
    #[spec(ty = ParamType::String)]
    sha256: Option<String>,
    /// Output filename, relative to the target's package. Defaults to the URL's
    /// last path segment (`…/heph-govet_linux_amd64` → `heph-govet_linux_amd64`),
    /// which is what the server is serving; set it to rename the fetched file.
    #[spec(ty = ParamType::String)]
    out: Option<String>,
    /// Mark the fetched file executable (a downloaded tool binary).
    executable: bool,
    /// Caching for the fetched file. Defaults to on for both the local and
    /// remote cache — a fetch is content-addressed (pinned by `sha256`), so it
    /// is safe to share. `cache = False` disables both tiers; the dict form
    /// `{enabled, remote, history}` toggles them independently (e.g.
    /// `cache = {"remote": False}` keeps the fetch local-only).
    cache: TargetSpecCache,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct HttpFetchDef {
    /// The *rendered* URL (placeholders already substituted from the addr args).
    url: String,
    sha256: String,
    /// Package-relative output path.
    out: String,
    executable: bool,
}

/// Bump to invalidate cached fetches when the output layout changes.
const HTTP_FETCH_FORMAT_VERSION: u32 = 1;

impl Hash for HttpFetchDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        HTTP_FETCH_FORMAT_VERSION.hash(state);
        self.url.hash(state);
        self.sha256.hash(state);
        self.out.hash(state);
        self.executable.hash(state);
    }
}

pub struct Driver;

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        HttpFetchSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = HttpFetchSpec::from(req.target_spec.config.clone())
            .context("parse http_fetch config")?;

        // Rendered here, not at run time: the URL an arg combination resolves to
        // is part of the target's identity, so it must feed the hash below.
        let url = render(&spec.url, &addr.args)
            .with_context(|| format!("render url of {}", addr.format()))?;

        let out_rel = match spec.out {
            Some(out) => out,
            None => file_name_of(&url)
                .with_context(|| format!("{url:?} has no file name — set `out` explicitly"))?,
        };
        let pkg = addr.package.as_str();
        let out = if pkg.is_empty() {
            out_rel
        } else {
            format!("{pkg}/{out_rel}")
        };

        let def = HttpFetchDef {
            url,
            sha256: spec.sha256.unwrap_or_default(),
            out: out.clone(),
            executable: spec.executable,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("http_fetch_{}", addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                // No inputs: the bytes come from the network, not from other
                // targets or the host filesystem.
                inputs: vec![],
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(out),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                support_files: vec![],
                cache: spec.cache.into(),
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
        let def = req.request.target.def_de::<HttpFetchDef>();
        // `out` is package-relative; the sandbox dir is the package's.
        let name = std::path::Path::new(&def.out)
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .ok_or_else(|| anyhow::anyhow!("http_fetch: out {:?} has no file name", def.out))?;
        let dest = req.sandbox_pkg_dir.join(name);

        let url = def.url.clone();
        let sha256 = def.sha256.clone();
        let executable = def.executable;

        // Download + verify + write is blocking IO; keep it off the async runtime.
        // Cancellation is honored by racing the join handle against the token (the
        // blocking work can't be interrupted mid-syscall, but we stop awaiting it).
        let work =
            tokio::task::spawn_blocking(move || fetch(&url, &sha256, executable, &dest.clone()));

        let fetch = async {
            work.await
                .context("http_fetch download task panicked")?
                .with_context(|| format!("fetch {}", def.url))
        };

        tokio::select! {
            r = fetch => r?,
            () = ctoken.cancelled() => anyhow::bail!("http_fetch: {} cancelled", def.url),
        }

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// The file name a URL fetches to: its last path segment, minus any `?query` /
/// `#fragment`. `None` when that segment is empty (`https://x/`, a bare host, a
/// path ending in `/`) — there is nothing to name the output after, so the target
/// must say `out` itself.
fn file_name_of(url: &str) -> Option<String> {
    // Drop the scheme so `https://host` (authority only, no path) has no segment
    // to mistake the host for.
    let authority_and_path = url.split_once("://").map_or(url, |(_scheme, rest)| rest);
    let path = authority_and_path.split(['?', '#']).next()?;
    let (_authority, path) = path.split_once('/')?;
    let name = path.rsplit('/').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Substitute `{arg}` placeholders in `template` from the target addr's `args`.
/// `{{` and `}}` are literal braces. An unknown placeholder is an error — a
/// silently-empty substitution would fetch the wrong URL and cache it.
fn render(template: &str, args: &BTreeMap<String, String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // `{{` / `}}` → one literal brace.
            '{' | '}' if chars.peek() == Some(&c) => {
                chars.next();
                out.push(c);
            }
            '}' => anyhow::bail!(
                "unmatched `}}` in url template {template:?} (write `}}}}` for a literal)"
            ),
            '{' => {
                let mut key = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    key.push(c);
                }
                if !closed {
                    anyhow::bail!("unclosed `{{` in url template {template:?}");
                }
                let value = args.get(&key).with_context(|| {
                    format!(
                        "url template {template:?} references arg `{key}`, which the addr does \
                         not set (has: {:?})",
                        args.keys().collect::<Vec<_>>()
                    )
                })?;
                out.push_str(value);
            }
            c => out.push(c),
        }
    }

    Ok(out)
}

/// Download `url`, verify it against `expected_sha256` (when non-empty), and
/// write it to `dest`. Pure blocking work.
fn fetch(
    url: &str,
    expected_sha256: &str,
    executable: bool,
    dest: &std::path::Path,
) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .context("build http client")?;
    let bytes = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .bytes()
        .with_context(|| format!("read body of {url}"))?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got = format!("{:x}", hasher.finalize());
    verify_checksum(expected_sha256, &got, url)?;

    std::fs::write(dest, &bytes).with_context(|| format!("write {dest:?}"))?;
    if executable {
        set_executable(dest).with_context(|| format!("chmod +x {dest:?}"))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(dest: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_dest: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

/// Compare the fetched bytes' `got` SHA-256 against the `expected` one. An empty
/// `expected` (no `sha256` on the target) fetches **unverified** — allowed, but
/// warned: the target's output is then whatever the server serves today. A
/// mismatch fails closed.
fn verify_checksum(expected: &str, got: &str, url: &str) -> anyhow::Result<()> {
    if expected.is_empty() {
        tracing::warn!(
            url,
            got,
            "http_fetch: downloading {url} without checksum verification — set `sha256` on the \
             target to pin its content (the fetched bytes hash to {got})"
        );
        return Ok(());
    }
    if got != expected {
        anyhow::bail!("http_fetch: checksum mismatch for {url}: expected {expected}, got {got}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_addr_args() {
        let got = render(
            "https://x/{tag}/tool_{goos}_{goarch}",
            &args(&[("tag", "v1.2.3"), ("goos", "darwin"), ("goarch", "arm64")]),
        )
        .expect("render");
        assert_eq!(got, "https://x/v1.2.3/tool_darwin_arm64");
    }

    #[test]
    fn render_passes_through_plain_urls_and_escaped_braces() {
        assert_eq!(
            render("https://x/y.tar.gz", &args(&[])).expect("render"),
            "https://x/y.tar.gz"
        );
        assert_eq!(
            render("https://x/{{literal}}", &args(&[])).expect("render"),
            "https://x/{literal}"
        );
    }

    #[test]
    fn render_rejects_unknown_arg() {
        // Fail closed: an empty substitution would fetch — and cache — the wrong
        // file under a hash that looks legitimate.
        let err = render("https://x/{goos}", &args(&[("goarch", "arm64")])).expect_err("unknown");
        let msg = format!("{err:#}");
        assert!(msg.contains("references arg `goos`"), "got: {msg}");
    }

    #[test]
    fn render_rejects_malformed_template() {
        assert!(render("https://x/{goos", &args(&[("goos", "linux")])).is_err());
        assert!(render("https://x/}", &args(&[])).is_err());
    }

    /// Serve `body` (with `status`) to the next `n` requests on an ephemeral
    /// loopback port; returns the base URL. Exercises the real fetch path
    /// (reqwest, status, checksum, write) with no outbound network.
    fn serve(status: &'static str, body: &'static [u8], n: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut sock, &mut buf);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
                let _ = std::io::Write::write_all(&mut sock, body);
            }
        });
        url
    }

    const BODY: &[u8] = b"\x7fELF-pretend-this-is-a-tool";
    /// sha256 of `BODY`.
    const BODY_SHA: &str = "a2e1a3b0f1b0d0e08b6ad0aad9f0f7cbb90b9f19e5e0ff5a1a5d0ba9e4a9e0f7";

    #[test]
    fn fetch_writes_executable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let url = serve("200 OK", BODY, 1);

        // Checksum unset → fetched unverified (warns); the bytes still land.
        fetch(&url, "", true, &dest).expect("fetch");

        assert_eq!(std::fs::read(&dest).expect("read"), BODY);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).expect("stat").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "must be executable");
        }
    }

    #[test]
    fn fetch_rejects_checksum_mismatch_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let url = serve("200 OK", b"tampered", 1);

        let err = fetch(&url, BODY_SHA, false, &dest).expect_err("mismatch must fail");
        assert!(
            format!("{err:#}").contains("checksum mismatch"),
            "got: {err:#}"
        );
        assert!(!dest.exists(), "a mismatched download must not be written");
    }

    #[test]
    fn fetch_fails_on_http_error_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let url = serve("404 Not Found", b"", 1);

        let err = fetch(&url, "", false, &dest).expect_err("404 must fail");
        assert!(format!("{err:#}").contains("GET"), "got: {err:#}");
    }

    fn parse_req(addr: &str, config: HashMap<String, Value>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr(addr).expect("addr"),
                driver: DRIVER_NAME.to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn file_name_of_takes_the_urls_last_segment() {
        assert_eq!(
            file_name_of("https://x/y/heph-govet_linux_amd64").as_deref(),
            Some("heph-govet_linux_amd64")
        );
        // Query/fragment are not part of the name.
        assert_eq!(
            file_name_of("https://x/y/tool.tar.gz?token=abc#frag").as_deref(),
            Some("tool.tar.gz")
        );
        // Nothing to name the file after → the target must say `out`.
        assert_eq!(file_name_of("https://x/y/"), None);
        assert_eq!(file_name_of("https://x"), None);
    }

    /// `out` defaults to the URL's last segment — the name the server serves the
    /// file under — and is written under the target's package.
    #[tokio::test]
    async fn parse_defaults_out_to_the_urls_last_segment() {
        let config = HashMap::from([(
            "url".to_string(),
            Value::String("https://x/rel/tool_{goos}.bin".to_string()),
        )]);
        let resp = Driver
            .parse(
                parse_req("//tools/dl:fetch@goos=linux", config),
                &StdCancellationToken::new(),
            )
            .await
            .expect("parse");

        let def = resp.target_def.def::<HttpFetchDef>();
        assert_eq!(def.url, "https://x/rel/tool_linux.bin");
        // Named after the URL, not after the target (`fetch`).
        assert_eq!(def.out, "tools/dl/tool_linux.bin");
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::FilePath(p) if p == "tools/dl/tool_linux.bin"
        ));
    }

    #[tokio::test]
    async fn parse_honors_an_explicit_out_and_renders_url_from_args() {
        let config = HashMap::from([
            (
                "url".to_string(),
                Value::String("https://x/{goos}/tool_{goos}".to_string()),
            ),
            ("out".to_string(), Value::String("tool".to_string())),
            ("executable".to_string(), Value::Bool(true)),
        ]);
        let req = parse_req("//tools/dl:tool@goos=linux", config);
        let resp = Driver
            .parse(req, &StdCancellationToken::new())
            .await
            .expect("parse");

        let def = resp.target_def.def::<HttpFetchDef>();
        assert_eq!(def.url, "https://x/linux/tool_linux");
        assert_eq!(def.out, "tools/dl/tool");
        assert!(def.executable);
    }

    #[tokio::test]
    async fn parse_fails_when_the_url_names_no_file_and_out_is_absent() {
        let config = HashMap::from([(
            "url".to_string(),
            Value::String("https://x/dir/".to_string()),
        )]);
        let err = Driver
            .parse(
                parse_req("//tools/dl:tool", config),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("no file name to default to");
        assert!(format!("{err:#}").contains("set `out`"), "got: {err:#}");
    }

    /// The rendered URL (not the template) feeds the hash, so the same target
    /// fetched for two platforms is two distinct cache entries.
    #[tokio::test]
    async fn parse_hash_differs_per_arg_combination() {
        let config = || {
            HashMap::from([(
                "url".to_string(),
                Value::String("https://x/tool_{goos}".to_string()),
            )])
        };
        let ctoken = StdCancellationToken::new();
        let linux = Driver
            .parse(parse_req("//tools/dl:tool@goos=linux", config()), &ctoken)
            .await
            .expect("parse linux");
        let darwin = Driver
            .parse(parse_req("//tools/dl:tool@goos=darwin", config()), &ctoken)
            .await
            .expect("parse darwin");

        assert_ne!(linux.target_def.hash, darwin.target_def.hash);
    }

    /// A fetch defaults to caching in both the local and remote tiers: the
    /// bytes are content-addressed, so they are safe to share.
    #[tokio::test]
    async fn parse_defaults_to_local_and_remote_cache() {
        let config = HashMap::from([(
            "url".to_string(),
            Value::String("https://x/tool".to_string()),
        )]);
        let resp = Driver
            .parse(
                parse_req("//tools/dl:tool", config),
                &StdCancellationToken::new(),
            )
            .await
            .expect("parse");

        let cache = resp.target_def.cache;
        assert!(cache.enabled, "local cache on by default");
        assert!(cache.remote_enabled, "remote cache on by default");
    }

    /// `cache = False` turns off both tiers.
    #[tokio::test]
    async fn parse_cache_false_disables_both_tiers() {
        let config = HashMap::from([
            (
                "url".to_string(),
                Value::String("https://x/tool".to_string()),
            ),
            ("cache".to_string(), Value::Bool(false)),
        ]);
        let resp = Driver
            .parse(
                parse_req("//tools/dl:tool", config),
                &StdCancellationToken::new(),
            )
            .await
            .expect("parse");

        let cache = resp.target_def.cache;
        assert!(!cache.enabled);
        assert!(!cache.remote_enabled);
    }

    /// The dict form toggles the tiers independently: keep the fetch local but
    /// off the remote cache.
    #[tokio::test]
    async fn parse_cache_dict_can_disable_remote_only() {
        let config = HashMap::from([
            (
                "url".to_string(),
                Value::String("https://x/tool".to_string()),
            ),
            (
                "cache".to_string(),
                Value::Map(HashMap::from([("remote".to_string(), Value::Bool(false))])),
            ),
        ]);
        let resp = Driver
            .parse(
                parse_req("//tools/dl:tool", config),
                &StdCancellationToken::new(),
            )
            .await
            .expect("parse");

        let cache = resp.target_def.cache;
        assert!(cache.enabled, "local stays on");
        assert!(!cache.remote_enabled, "remote disabled");
    }

    #[tokio::test]
    async fn parse_fails_on_unresolvable_placeholder() {
        let config = HashMap::from([(
            "url".to_string(),
            Value::String("https://x/tool_{goos}".to_string()),
        )]);
        let err = Driver
            .parse(
                parse_req("//tools/dl:tool", config),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("unresolvable placeholder must fail parse");
        assert!(
            format!("{err:#}").contains("references arg `goos`"),
            "got: {err:#}"
        );
    }
}
