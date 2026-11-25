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
//! `sha256` (when set) is verified before the target succeeds — a mismatched
//! file is removed again — so a changed remote asset fails the build closed
//! instead of poisoning the cache.

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
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use xxhash_rust::xxh3::Xxh3Default;

mod auth;

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
    /// Credentials to fetch with, as `credential` target addresses presented
    /// with `heph.auth.netrc([...])`. The URL's host is sent the basic auth of
    /// the netrc entry naming it; a host none of them covers is an error, never
    /// an anonymous request. When empty, the fetch is anonymous: the host's own
    /// `~/.netrc` is not read.
    ///
    /// Not an input: nothing about a credential reaches the cache key. So a
    /// credential-gated fetch is only as cacheable as it is pinned — without
    /// `sha256`, two identities served different bytes would share one entry.
    /// Set `sha256`, or `cache = False`.
    credentials: Vec<String>,
    /// Caching for the fetched file. Defaults to on for both the local and
    /// remote cache — a fetch is content-addressed (pinned by `sha256`), so it
    /// is safe to share. `cache = False` disables both tiers; the dict form
    /// `{enabled, remote, history}` toggles them independently (e.g.
    /// `cache = {"remote": False}` keeps the fetch local-only).
    cache: TargetSpecCache,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. A fetch waits on the network, so the driver advertises
    /// `cpu` 0.1 ([`Resources::IO_CPU`](hplugin::driver::Resources::IO_CPU))
    /// when the target declares none. Never hashed.
    resources: hplugin::driver::Resources,
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
        let spec =
            HttpFetchSpec::from(&req.target_spec.config).context("parse http_fetch config")?;

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

        let credential_inputs =
            hdriver_support::credential::inputs(&spec.credentials, &addr.package)?;
        let cache: hplugin::driver::targetdef::CacheConfig = spec.cache.into();
        let sha256 = spec.sha256.unwrap_or_default();
        if !credential_inputs.is_empty() {
            // Two identities for one request: the one in the URL would be sent,
            // or not, depending on what the client does with both.
            anyhow::ensure!(
                !has_userinfo(&url),
                "{}: the url carries a username or password and the target names \
                 `credentials`; drop the one from the url",
                addr.format()
            );
            // The rendered URL is the whole key, and a server can answer it
            // differently per identity: without a pin, a shared entry serves
            // one identity's bytes to another.
            if sha256.is_empty() && cache.enabled {
                tracing::warn!(
                    url,
                    "http_fetch: {url} is fetched with credentials and no `sha256`, so what it \
                     serves can depend on whichever identity ran it, and the cache entry (remote \
                     included) is shared across identities; set `sha256`, or `cache = False`"
                );
            }
        }

        let def = HttpFetchDef {
            url,
            sha256,
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
                // The bytes come from the network, not from other targets or
                // the host filesystem. The only edges are credentials, which
                // materialize nothing and never reach the cache key.
                inputs: credential_inputs,
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(out),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                cache,
                hash,
                // Not in `hash` above: a declaration moves no key.
                resources: spec
                    .resources
                    .advertise(
                        hplugin::provider::PoolName::cpu(),
                        hplugin::driver::Resources::IO_CPU,
                    )
                    .context("http resources")?,
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

        // Off the async workers: it reads the presented netrc files.
        let declared = auth::declared(req.request.target);
        let selected = if declared == 0 && req.request.credentials.is_empty() {
            None
        } else {
            let target = req.request.target.addr.format();
            let mounts = req.request.credentials.clone();
            let url = def.url.clone();
            hcore::blocking::run(move || auth::resolve(&target, declared, &mounts, &url)).await?
        };

        let fetched = fetch_until_cancelled(
            &def.url,
            selected.as_ref().map(|s| &s.auth),
            &def.sha256,
            def.executable,
            &dest,
            ctoken,
        )
        .await;
        match selected {
            Some(s) => fetched.with_context(|| {
                format!(
                    "authenticated with the netrc entry credential {} presents",
                    s.credential
                )
            })?,
            None => fetched?,
        }

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Whether `url` names a username or password in its authority
/// (`https://user:pass@host/`).
fn has_userinfo(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| !u.username().is_empty() || u.password().is_some())
}

/// [`fetch`], abandoned the moment `ctoken` fires.
///
/// The transfer runs on the host's runtime (this driver is built into the
/// binary, never a cdylib), so cancelling drops it: the transfer stops, the
/// connection closes, and the partial file is removed before this returns.
async fn fetch_until_cancelled(
    url: &str,
    auth: Option<&hfetch::BasicAuth>,
    expected_sha256: &str,
    executable: bool,
    dest: &std::path::Path,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<()> {
    match fetch(
        url,
        auth,
        expected_sha256,
        executable,
        dest,
        ctoken.cancelled(),
    )
    .await
    {
        Ok(Fetched::Done) => Ok(()),
        Ok(Fetched::Cancelled) => anyhow::bail!("http_fetch: {url} cancelled"),
        Err(err) => Err(err).with_context(|| format!("fetch {url}")),
    }
}

/// How [`fetch`] ended, when it did not fail.
#[derive(Debug, PartialEq, Eq)]
enum Fetched {
    Done,
    /// The cancel signal fired first. Nothing is left at `dest`.
    Cancelled,
}

/// Remove what an abandoned or failed download left at `dest`, if anything.
fn remove_partial(dest: &std::path::Path) {
    if let Err(rm_err) = std::fs::remove_file(dest)
        && rm_err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(path = ?dest, error = %rm_err, "remove failed download");
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
/// write it to `dest`. The body is streamed to `dest` and hashed on the way; a
/// failed download, a mismatch or `cancelled` firing removes it again.
async fn fetch(
    url: &str,
    auth: Option<&hfetch::BasicAuth>,
    expected_sha256: &str,
    executable: bool,
    dest: &std::path::Path,
    cancelled: impl Future<Output = ()>,
) -> anyhow::Result<Fetched> {
    let verified = match download(url, auth, dest, cancelled).await {
        Ok(Some(got)) => verify_checksum(expected_sha256, &got, url),
        Ok(None) => {
            remove_partial(dest);
            return Ok(Fetched::Cancelled);
        }
        Err(err) => Err(err),
    };
    if let Err(err) = verified {
        remove_partial(dest);
        return Err(err);
    }

    if executable {
        set_executable(dest).with_context(|| format!("chmod +x {dest:?}"))?;
    }
    Ok(Fetched::Done)
}

/// How many batches of the body may wait for the file writer, on top of the
/// one it is writing.
const WRITE_QUEUE: usize = 1;

/// Received bytes the transfer gathers, while the writer is busy, before it
/// waits for the writer.
const WRITE_BATCH: usize = 1024 * 1024;

/// The file a body is written into, and the hash of what has landed in it.
type BodyFile = hfetch::Sha256Writer<std::io::BufWriter<std::fs::File>>;

/// The transfer's end of the channel to the writer. Unless the transfer marked
/// the body `whole`, dropping it sets `abandoned` before the sender goes, so a
/// writer woken by the channel closing already sees the flag.
struct Feed<'a, T> {
    tx: tokio::sync::mpsc::Sender<T>,
    abandoned: &'a AtomicBool,
    whole: bool,
}

impl<T> Feed<'_, T> {
    /// The whole body has been handed to the writer: dropping this no longer
    /// abandons the download.
    fn received_whole(&mut self) {
        self.whole = true;
    }
}

impl<T> Drop for Feed<'_, T> {
    fn drop(&mut self) {
        // `drop` runs before the fields are dropped, so this happens before
        // the sender closes the channel.
        if !self.whole {
            self.abandoned.store(true, Ordering::Release);
        }
    }
}

/// Stream `url` into a new file at `dest` and return the body's SHA-256, or
/// `None` when `cancelled` fired first.
///
/// The transfer runs on the caller's runtime, so `cancelled` drops it and
/// closes the connection. It hands the body to the writer in batches over a
/// bounded channel: whatever has arrived as soon as the writer can take it, at
/// most [`WRITE_BATCH`] bytes gathered while it cannot, so a slow disk holds
/// the transfer back rather than queuing the body in memory.
///
/// Creating the file, writing a batch and finishing the hash are each one short
/// job on the blocking pool, so a runtime worker never waits on the disk. The
/// file and its hasher wait in the writer between jobs: a download waiting on
/// the network holds no blocking slot, which cache IO, sqlite reads and
/// Starlark evaluation need too.
///
/// Every way out (end of body, failure, cancel) drops the sender, which ends
/// the writer once its last job has finished, and this returns only once the
/// writer has ended — so the caller can remove what it wrote without racing it.
/// A transfer that did not receive the whole body marks itself abandoned
/// first, and the writer starts no further job: the file is about to be
/// removed, so writing the queued batch or hashing it would only take a
/// blocking slot.
async fn download(
    url: &str,
    auth: Option<&hfetch::BasicAuth>,
    dest: &std::path::Path,
    cancelled: impl Future<Output = ()>,
) -> anyhow::Result<Option<String>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(WRITE_QUEUE);
    let abandoned = AtomicBool::new(false);
    let abandoned = &abandoned;
    let writer = async move {
        let gone = || anyhow::anyhow!("download to {dest:?} abandoned");
        if abandoned.load(Ordering::Acquire) {
            return Err(gone());
        }
        let mut file = hcore::blocking::run({
            let dest = dest.to_owned();
            move || -> anyhow::Result<BodyFile> {
                let f = std::fs::File::create(&dest).with_context(|| format!("create {dest:?}"))?;
                Ok(hfetch::Sha256Writer::new(std::io::BufWriter::new(f)))
            }
        })
        .await?;
        let job_url: std::sync::Arc<str> = url.into();
        while let Some(batch) = rx.recv().await {
            if abandoned.load(Ordering::Acquire) {
                return Err(gone());
            }
            let url = job_url.clone();
            file = hcore::blocking::run(move || {
                file.write_chunks(&url, batch)?;
                anyhow::Ok(file)
            })
            .await
            .with_context(|| format!("download to {dest:?}"))?;
        }
        // The channel closed: the sender was dropped, after any abandon.
        if abandoned.load(Ordering::Acquire) {
            return Err(gone());
        }
        hcore::blocking::run(move || file.finish(&job_url))
            .await
            .with_context(|| format!("download to {dest:?}"))
    };
    // Built here, not inside `send_body`, so a transfer dropped before its
    // first poll still marks itself abandoned.
    let mut feed = Feed {
        tx,
        abandoned,
        whole: false,
    };
    // Owns `feed`, so the sender is dropped the moment the transfer ends or
    // is dropped.
    let send_body = async move {
        let tx = &feed.tx;
        let mut body = hfetch::Fetcher::default().open_with(url, auth).await?;
        let mut batch = Vec::new();
        let mut batched = 0;
        loop {
            // Hand the batch over the moment the writer can take it, and
            // keep receiving meanwhile up to `WRITE_BATCH`. `Body::chunk` is
            // cancel-safe, so losing the race to `reserve` drops no bytes.
            tokio::select! {
                biased;
                permit = tx.reserve(), if !batch.is_empty() => {
                    // The writer has gone: it failed, and its error is the
                    // one to report.
                    let Ok(permit) = permit else {
                        return anyhow::Ok(());
                    };
                    permit.send(std::mem::take(&mut batch));
                    batched = 0;
                }
                // Never both disabled: a full batch is not empty.
                chunk = body.chunk(), if batched < WRITE_BATCH => {
                    let Some(chunk) = chunk? else { break };
                    batched += chunk.len();
                    batch.push(chunk);
                }
            }
        }
        // As above, a writer that has gone has its own error to report.
        if !batch.is_empty() && tx.send(batch).await.is_err() {
            return anyhow::Ok(());
        }
        feed.received_whole();
        anyhow::Ok(())
    };
    let transfer = async {
        tokio::select! {
            r = send_body => Some(r.with_context(|| format!("download to {dest:?}"))),
            () = cancelled => None,
        }
    };
    let (sent, written) = tokio::join!(transfer, writer);
    match sent {
        None => Ok(None),
        // A transfer that failed fails the download, whatever the writer did
        // with the part it got.
        Some(Err(err)) => Err(err),
        Some(Ok(())) => written.map(Some),
    }
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

    #[tokio::test]
    async fn fetch_writes_executable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let url = serve("200 OK", BODY, 1);

        // Checksum unset → fetched unverified (warns); the bytes still land.
        let fetched = fetch(&url, None, "", true, &dest, std::future::pending())
            .await
            .expect("fetch");
        assert_eq!(fetched, Fetched::Done);

        assert_eq!(std::fs::read(&dest).expect("read"), BODY);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).expect("stat").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "must be executable");
        }
    }

    /// A body several write batches long lands whole, in order, and verifies
    /// against its checksum.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_writes_a_body_larger_than_a_batch() {
        let body: &'static [u8] = (0..3 * WRITE_BATCH + 17)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>()
            .leak();
        let (sha, _len) = hfetch::sha256_hex_of(body).expect("hash");
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let url = serve("200 OK", body, 1);

        let fetched = fetch(&url, None, &sha, false, &dest, std::future::pending())
            .await
            .expect("fetch");
        assert_eq!(fetched, Fetched::Done);
        assert!(std::fs::read(&dest).expect("read") == body, "body differs");
    }

    #[tokio::test]
    async fn fetch_rejects_checksum_mismatch_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let url = serve("200 OK", b"tampered", 1);

        let err = fetch(&url, None, BODY_SHA, false, &dest, std::future::pending())
            .await
            .expect_err("mismatch must fail");
        assert!(
            format!("{err:#}").contains("checksum mismatch"),
            "got: {err:#}"
        );
        assert!(!dest.exists(), "a mismatched download must not be written");
    }

    #[tokio::test]
    async fn fetch_fails_on_http_error_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let url = serve("404 Not Found", b"", 1);

        let err = fetch(&url, None, "", false, &dest, std::future::pending())
            .await
            .expect_err("404 must fail");
        assert!(format!("{err:#}").contains("GET"), "got: {err:#}");
    }

    /// Cancelling mid-transfer stops the download: the run returns at once,
    /// the server sees its connection closed, and no partial file is left.
    /// The download used to run on a blocking thread the token could not
    /// reach, so it held the connection until the body ended or timed out.
    #[tokio::test]
    async fn cancel_mid_transfer_closes_the_connection_and_removes_the_file() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/tool", listener.local_addr().expect("addr"));
        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut sock, &mut buf);
            // Announce far more than is ever sent, so the body never ends.
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 100000000\r\n\r\n";
            let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
            let _ = std::io::Write::write_all(&mut sock, &[0u8; 64 * 1024]);
            let _ = sent_tx.send(());
            // Blocks until the client closes its end (EOF) — or never.
            let closed = matches!(std::io::Read::read(&mut sock, &mut buf), Ok(0) | Err(_));
            let _ = closed_tx.send(closed);
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let ctoken = hcore::hasync::StdCancellationToken::new();
        let cancel = ctoken.clone();
        tokio::spawn(async move {
            let _ = sent_rx.await;
            cancel.cancel();
        });

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fetch_until_cancelled(&url, None, "", false, &dest, &ctoken),
        )
        .await
        .expect("a cancelled fetch returns at once")
        .expect_err("a cancelled fetch fails");
        assert!(format!("{err:#}").contains("cancelled"), "got: {err:#}");
        assert!(!dest.exists(), "a cancelled download must not leave a file");
        let closed = tokio::task::spawn_blocking(move || {
            closed_rx.recv_timeout(std::time::Duration::from_secs(10))
        })
        .await
        .expect("join");
        assert_eq!(closed, Ok(true), "cancel must close the connection");
    }

    /// Serve a body that never ends: `sent` bytes, then hold the connection
    /// until the client closes it. Returns the URL.
    fn serve_endless(sent: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/tool", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut sock, &mut buf);
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 100000000\r\n\r\n";
            let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
            let _ = std::io::Write::write_all(&mut sock, &vec![0u8; sent]);
            let _ = std::io::Read::read(&mut sock, &mut buf);
        });
        url
    }

    /// Cancelling once the writer has put bytes on disk: the run returns, and
    /// the partial file is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_mid_write_removes_the_partial_file() {
        let url = serve_endless(256 * 1024);
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let ctoken = hcore::hasync::StdCancellationToken::new();
        let cancel = ctoken.clone();
        let watched = dest.clone();
        tokio::spawn(async move {
            while std::fs::metadata(&watched).map_or(0, |m| m.len()) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            cancel.cancel();
        });

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fetch_until_cancelled(&url, None, "", false, &dest, &ctoken),
        )
        .await
        .expect("a cancelled fetch returns at once")
        .expect_err("a cancelled fetch fails");
        assert!(format!("{err:#}").contains("cancelled"), "got: {err:#}");
        assert!(!dest.exists(), "a cancelled download must not leave a file");
    }

    /// Every byte received reaches the disk while the network stalls. Bytes
    /// that arrived while the writer was busy used to wait in the transfer for
    /// the next chunk, which a stalled network never sends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_body_reaches_the_disk_in_full() {
        const SENT: usize = 256 * 1024;
        let url = serve_endless(SENT);
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let ctoken = StdCancellationToken::new();
        let download = tokio::spawn({
            let (dest, ctoken) = (dest.clone(), ctoken.clone());
            async move { fetch_until_cancelled(&url, None, "", false, &dest, &ctoken).await }
        });
        let on_disk = || std::fs::metadata(&dest).map_or(0, |m| m.len());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while on_disk() < SENT as u64 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let landed = on_disk();

        ctoken.cancel();
        let err = download
            .await
            .expect("join")
            .expect_err("a cancelled fetch fails");
        assert!(format!("{err:#}").contains("cancelled"), "got: {err:#}");
        assert_eq!(
            landed, SENT as u64,
            "every byte received must reach the disk while the network stalls"
        );
        assert!(!dest.exists(), "a cancelled download must not leave a file");
    }

    /// A cancelled download starts no further blocking job: its file is about
    /// to be removed, so flushing and hashing it would only take a slot. With
    /// every slot held, the cancel still returns at once. The writer used to
    /// queue `finish` for a slot, and the fetch waited for it before removing
    /// the file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_download_starts_no_blocking_job() {
        let url = serve_endless(0);
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let ctoken = StdCancellationToken::new();
        let download = tokio::spawn({
            let (dest, ctoken) = (dest.clone(), ctoken.clone());
            async move { fetch_until_cancelled(&url, None, "", false, &dest, &ctoken).await }
        });
        // The writer has created the file and waits on a body that never
        // starts.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !dest.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the download creates its file");

        // Hold every blocking slot until `releases` is dropped.
        let n = hcore::blocking::concurrency_limit();
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut releases = Vec::with_capacity(n);
        let mut holders = tokio::task::JoinSet::new();
        for _ in 0..n {
            let (release, held) = std::sync::mpsc::channel::<()>();
            releases.push(release);
            let started = started.clone();
            holders.spawn(hcore::blocking::run(move || {
                started.fetch_add(1, Ordering::SeqCst);
                while held.recv().is_ok() {}
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while started.load(Ordering::SeqCst) < n {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("every blocking slot is held");

        ctoken.cancel();
        let returned = tokio::time::timeout(std::time::Duration::from_secs(5), download).await;
        drop(releases);
        while let Some(joined) = holders.join_next().await {
            joined.expect("join");
        }
        let err = returned
            .expect("a cancelled download needs no blocking slot to return")
            .expect("join")
            .expect_err("a cancelled fetch fails");
        assert!(format!("{err:#}").contains("cancelled"), "got: {err:#}");
        assert!(!dest.exists(), "a cancelled download must not leave a file");
    }

    /// Cancelled before the writer job has run: the file the job then creates
    /// is still removed, because the fetch waits for the job before removing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_before_the_writer_runs_leaves_no_file_behind() {
        let url = serve_endless(0);
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");
        let ctoken = hcore::hasync::StdCancellationToken::new();
        ctoken.cancel();

        let err = fetch_until_cancelled(&url, None, "", false, &dest, &ctoken)
            .await
            .expect_err("a cancelled fetch fails");
        assert!(format!("{err:#}").contains("cancelled"), "got: {err:#}");
        // A job still running after the fetch returned would create the file
        // now; give it the time to.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!dest.exists(), "a cancelled download must not leave a file");
    }

    /// A download waiting on the network holds no blocking slot. With as many
    /// stalled downloads in flight as there are slots, another blocking job
    /// still runs at once. The writer used to park in its slot for the whole
    /// transfer, so a few slow downloads starved cache IO, sqlite reads and
    /// Starlark evaluation of every slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_downloads_hold_no_blocking_slot() {
        let n = hcore::blocking::concurrency_limit();
        let dir = tempfile::tempdir().expect("tempdir");
        let ctoken = StdCancellationToken::new();
        let mut downloads = tokio::task::JoinSet::new();
        let mut dests = Vec::with_capacity(n);
        for i in 0..n {
            let url = serve_endless(64 * 1024);
            let dest = dir.path().join(format!("tool-{i}"));
            dests.push(dest.clone());
            let ctoken = ctoken.clone();
            downloads.spawn(async move {
                fetch_until_cancelled(&url, None, "", false, &dest, &ctoken).await
            });
        }
        // Every download has created its file and now waits on a body that
        // never ends. Waiting for the file, not for bytes in it: a writer that
        // kept its slot for the whole transfer created the file inside that
        // slot too, so it fails the assertion below rather than this wait.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for dest in &dests {
                while !dest.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        })
        .await
        .expect("every download creates its file");

        let other = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            hcore::blocking::run(|| 42),
        )
        .await;

        ctoken.cancel();
        while let Some(joined) = downloads.join_next().await {
            let err = joined.expect("join").expect_err("a cancelled fetch fails");
            assert!(format!("{err:#}").contains("cancelled"), "got: {err:#}");
        }
        assert_eq!(
            other,
            Ok(42),
            "a blocking job must not wait for {n} stalled downloads"
        );
        for dest in &dests {
            assert!(!dest.exists(), "a cancelled download must not leave a file");
        }
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

    /// A fetch that declares no `cpu` advertises 0.1; a declared `cpu` replaces it, and
    /// neither moves the def hash.
    #[tokio::test]
    async fn resources_default_to_a_tenth_of_a_cpu_and_are_unhashed() {
        let parse = |resources: Option<Value>| async move {
            let mut config = HashMap::from([(
                "url".to_string(),
                Value::String("https://x/tool.bin".to_string()),
            )]);
            if let Some(r) = resources {
                config.insert("resources".to_string(), r);
            }
            Driver
                .parse(parse_req("//t:fetch", config), &StdCancellationToken::new())
                .await
                .expect("parse")
                .target_def
        };
        let plain = parse(None).await;
        assert_eq!(plain.resources.declared("cpu"), Some("0.1"));
        let heavy = parse(Some(Value::Map(
            [("cpu".to_string(), Value::Int(2))].into(),
        )))
        .await;
        assert_eq!(heavy.resources.declared("cpu"), Some("2"));
        assert_eq!(heavy.hash, plain.hash);
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

    async fn parse_with_credentials(url: &str, credentials: &[&str]) -> anyhow::Result<TargetDef> {
        let mut config = HashMap::from([
            ("url".to_string(), Value::String(url.to_string())),
            ("sha256".to_string(), Value::String(BODY_SHA.to_string())),
        ]);
        if !credentials.is_empty() {
            config.insert(
                "credentials".to_string(),
                Value::List(
                    credentials
                        .iter()
                        .map(|c| Value::String((*c).to_string()))
                        .collect(),
                ),
            );
        }
        Ok(Driver
            .parse(parse_req("//dl:tool", config), &StdCancellationToken::new())
            .await?
            .target_def)
    }

    /// A credential is an edge the cache key cannot see: annotated, neither
    /// hashed nor materialized at runtime, resolved against the package, and
    /// adding one leaves the def hash where it was.
    #[tokio::test]
    async fn credentials_are_unhashed_edges() {
        let plain = parse_with_credentials("https://x/tool", &[])
            .await
            .expect("parse");
        assert!(plain.inputs.is_empty());
        let gated = parse_with_credentials("https://x/tool", &[":netrc"])
            .await
            .expect("parse");
        let [edge] = gated.inputs.as_slice() else {
            panic!("one credential edge, got {}", gated.inputs.len());
        };
        assert!(hdriver_support::credential::is_credential(
            &edge.annotations
        ));
        assert!(!edge.hashed && !edge.runtime);
        assert_eq!(edge.r#ref.to_string(), "//dl:netrc");
        assert_eq!(gated.hash, plain.hash, "a credential must not move the key");
    }

    /// A URL naming its own user and a declared credential are two identities
    /// for one request; parse refuses the pair rather than let the client pick.
    #[tokio::test]
    async fn userinfo_in_the_url_and_credentials_are_exclusive() {
        let Err(err) = parse_with_credentials("https://u:p@x/tool", &["//auth:netrc"]).await else {
            panic!("two identities must fail parse");
        };
        assert!(
            format!("{err:#}").contains("url carries a username or password"),
            "{err:#}"
        );
        parse_with_credentials("https://u:p@x/tool", &[])
            .await
            .expect("userinfo alone is the author's own business");
    }

    /// Serve `body` to one request, recording the `Authorization` header it
    /// arrived with (empty when none).
    fn serve_recording_auth(body: &'static [u8]) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let n = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            let auth = head
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                        .map(|(_, v)| v.trim().to_string())
                })
                .unwrap_or_default();
            let _ = tx.send(auth);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = std::io::Write::write_all(&mut sock, resp.as_bytes());
            let _ = std::io::Write::write_all(&mut sock, body);
        });
        (url, rx)
    }

    /// The selected entry goes out as basic auth, and nothing goes out without
    /// one.
    #[tokio::test]
    async fn fetch_sends_basic_auth_only_when_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");

        let (url, auth) = serve_recording_auth(BODY);
        let creds = hfetch::BasicAuth {
            username: "u".to_string(),
            password: "p".to_string(),
        };
        fetch(&url, Some(&creds), "", false, &dest, std::future::pending())
            .await
            .expect("fetch");
        // base64("u:p")
        assert_eq!(auth.recv().expect("request"), "Basic dTpw");

        let (url, auth) = serve_recording_auth(BODY);
        fetch(&url, None, "", false, &dest, std::future::pending())
            .await
            .expect("fetch");
        assert_eq!(auth.recv().expect("request"), "");
    }
}
