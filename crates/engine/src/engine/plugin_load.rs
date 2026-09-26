//! Resolving the `plugins:` config into a live registry: applying built-ins,
//! and resolving + downloading + loading cdylib plugins from their
//! `*-plugin.json` manifests. The bin builds the engine and registers the
//! in-process plugin factories; the engine owns turning config into loaded
//! plugins (here), including the download cache under `~/.heph/plugins`.

use anyhow::Context;

use super::Engine;
use super::config_yaml;

/// One resolved `path:`/`url:` plugin entry awaiting dylib load: its source, the
/// optional manifest checksum (`url:` only), and the structured options for the
/// cdylib's create entry.
struct ManifestPlugin {
    identifier: config_yaml::PluginIdentifier,
    checksum: Option<String>,
    options: std::collections::HashMap<String, hplugin_abi::pb::Value>,
}

impl Engine {
    /// Apply every `plugins:` entry. `builtin:` entries instantiate the matching
    /// in-process factory (registered before this call); `path:`/`url:` entries
    /// resolve a `*-plugin.json` manifest, load the cdylib it names for this
    /// host, and register the provider + drivers it exports (the plugin
    /// self-describes its names via `config()`).
    pub fn register_plugins(&mut self, plugins: &[config_yaml::PluginSpec]) -> anyhow::Result<()> {
        let mut manifests: Vec<ManifestPlugin> = Vec::new();
        for spec in plugins {
            match &spec.identifier {
                config_yaml::PluginIdentifier::Builtin(name) => self
                    .apply_builtin(name, &spec.options)
                    .with_context(|| format!("apply builtin plugin `{name}`"))?,
                // Manifest plugin: carry the options as structured pb map data for
                // the cdylib's create entry (pb::CreateConfig), resolve + load below.
                _ => manifests.push(ManifestPlugin {
                    identifier: spec.identifier.clone(),
                    checksum: spec.checksum.clone(),
                    options: hplugin_abi::convert::options_to_pb_map(&spec.options),
                }),
            }
        }
        let root = self.cfg.root.clone();
        let home = self.home.clone();
        load_dylib_plugins(self, &root, &home, manifests)
    }
}

/// One plugin distribution manifest (`*-plugin.json`): a name + the dylib to load
/// per os/arch.
#[cfg(unix)]
#[derive(serde::Deserialize)]
struct PluginManifest {
    name: String,
    #[serde(default)]
    artifacts: Vec<ManifestArtifact>,
}

#[cfg(unix)]
#[derive(serde::Deserialize)]
struct ManifestArtifact {
    os: String,
    arch: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// Expected checksum of the resolved artifact (`sha256:<hex>`). When set, the
    /// loaded dylib bytes are verified against it before use. `None` skips the
    /// check. Applies to both `path:` and `url:` artifacts.
    #[serde(default)]
    checksum: Option<String>,
}

/// Resolve each manifest to a host dylib, load it (parallel — loading constructs
/// the plugin, which isn't free), and register its exported components directly.
#[cfg(unix)]
fn load_dylib_plugins(
    e: &mut Engine,
    root: &std::path::Path,
    home_dir: &std::path::Path,
    manifests: Vec<ManifestPlugin>,
) -> anyhow::Result<()> {
    use rayon::prelude::*;

    if manifests.is_empty() {
        return Ok(());
    }
    let root_str = root.to_string_lossy().into_owned();
    let home_str = home_dir.to_string_lossy().into_owned();
    let loaded = manifests
        .into_par_iter()
        .map(|m| -> anyhow::Result<_> {
            let dylib = resolve_manifest_dylib(&m.identifier, m.checksum.as_deref(), root)?;
            hplugin_stabby::load_stable::load(&dylib, &root_str, &home_str, m.options)
                .with_context(|| format!("load plugin dylib {}", dylib.display()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Engine mutation is single-threaded; register the loaded components in order.
    for (provider, drivers, hooks, runners) in loaded {
        if let Some(p) = provider {
            e.register_provider(|_| Box::new(p))?;
        }
        for (_name, drv) in drivers {
            e.register_managed_driver(|_| Box::new(drv))?;
        }
        for (_name, hook) in hooks {
            e.register_hook(std::sync::Arc::new(hook))?;
        }
        for runner in runners {
            e.register_exec_runner(runner)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn load_dylib_plugins(
    _e: &mut Engine,
    _root: &std::path::Path,
    _home_dir: &std::path::Path,
    manifests: Vec<ManifestPlugin>,
) -> anyhow::Result<()> {
    if manifests.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("`path:`/`url:` (cdylib) plugins are only supported on unix")
    }
}

/// Normalize a config/manifest path to an absolute path against `base`. First
/// expand a leading `~` / `~/` to `$HOME` (a bare `~` or `~/rest`; `~user` and
/// embedded `~` are left untouched, and so is everything if `HOME` is unset),
/// then if the result is still relative join it onto `base`.
///
/// So `~/.heph/x` → `$HOME/.heph/x`, `/abs` → `/abs`, `rel` → `base/rel`.
#[cfg(unix)]
fn resolve_path(p: &str, base: &std::path::Path) -> std::path::PathBuf {
    let expanded = match std::env::var_os("HOME") {
        Some(home) if p == "~" => std::path::PathBuf::from(home),
        // Only a real `~/` prefix joins onto $HOME.
        Some(home) => match p.strip_prefix("~/") {
            Some(rest) => std::path::PathBuf::from(home).join(rest),
            None => std::path::PathBuf::from(p),
        },
        None => std::path::PathBuf::from(p),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

/// Resolve a plugin manifest source to the concrete dylib path for this host:
/// load the manifest (local read or download), pick the artifact matching the
/// host os/arch, then resolve that artifact (a local `path` sibling of the
/// manifest, or a `url` to download + cache).
#[cfg(unix)]
fn resolve_manifest_dylib(
    src: &config_yaml::PluginIdentifier,
    manifest_checksum: Option<&str>,
    root: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let (manifest_bytes, manifest_dir) = match src {
        config_yaml::PluginIdentifier::Path(p) => {
            let mp = resolve_path(p, root);
            let bytes = std::fs::read(&mp)
                .with_context(|| format!("read plugin manifest {}", mp.display()))?;
            let dir = mp
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| root.to_path_buf());
            (bytes, dir)
        }
        config_yaml::PluginIdentifier::Url(u) => {
            // Manifest json is cached under ~/.heph/plugins alongside its artifacts.
            let mp = download_plugin(u)?;
            let bytes = std::fs::read(&mp)
                .with_context(|| format!("read downloaded plugin manifest {}", mp.display()))?;
            let dir = mp
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            (bytes, dir)
        }
        config_yaml::PluginIdentifier::Builtin(_) => {
            anyhow::bail!("internal: builtin plugin reached manifest resolution")
        }
    };

    // Verify the manifest against the config-pinned checksum before trusting any
    // artifact URL/checksum it declares — the manifest is the root of the chain.
    if let Some(expected) = manifest_checksum {
        verify_checksum(&manifest_bytes, expected).context("verify plugin manifest checksum")?;
    }

    let manifest: PluginManifest =
        serde_json::from_slice(&manifest_bytes).context("parse plugin manifest json")?;
    let (os, arch) = host_os_arch();
    let art = manifest
        .artifacts
        .iter()
        .find(|a| a.os == os && a.arch == arch)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "plugin `{}` has no artifact for this host ({os}/{arch})",
                manifest.name
            )
        })?;
    let dylib = match (&art.path, &art.url) {
        (Some(p), None) => resolve_path(p, &manifest_dir),
        (None, Some(u)) => download_plugin(u)?,
        _ => anyhow::bail!(
            "plugin `{}` artifact for {os}/{arch} must set exactly one of `path`/`url`",
            manifest.name
        ),
    };

    // Verify the resolved dylib (downloaded or local) against the manifest's
    // per-artifact checksum, if one is declared.
    if let Some(expected) = art.checksum.as_deref() {
        let bytes = std::fs::read(&dylib)
            .with_context(|| format!("read plugin artifact {} for checksum", dylib.display()))?;
        verify_checksum(&bytes, expected).with_context(|| {
            format!(
                "verify checksum of plugin `{}` artifact {}",
                manifest.name,
                dylib.display()
            )
        })?;
    }
    Ok(dylib)
}

/// Verify `bytes` against an `algo:hex` checksum spec (currently only
/// `sha256:<hex>`). Errors on an unknown/malformed spec or a digest mismatch.
#[cfg(unix)]
fn verify_checksum(bytes: &[u8], expected: &str) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    let (algo, want_hex) = expected
        .split_once(':')
        .with_context(|| format!("checksum must be `algo:hex`, got {expected:?}"))?;
    if !algo.eq_ignore_ascii_case("sha256") {
        anyhow::bail!("unsupported checksum algorithm {algo:?} (only sha256)");
    }
    let want = hex::decode(want_hex)
        .with_context(|| format!("checksum hex is not valid hex: {want_hex:?}"))?;
    let got = Sha256::digest(bytes);
    if got.as_slice() != want.as_slice() {
        anyhow::bail!(
            "checksum mismatch: expected sha256:{want_hex}, got sha256:{}",
            hex::encode(got)
        );
    }
    Ok(())
}

/// Host os/arch in the published-artifact spelling (`darwin`/`linux`,
/// `amd64`/`arm64`), matching the manifest's `artifacts[].{os,arch}`.
#[cfg(unix)]
fn host_os_arch() -> (String, String) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    (os.to_string(), arch.to_string())
}

/// An exclusive, advisory, cross-process file lock (`flock(2)`), released on drop.
/// Serializes concurrent plugin downloads of the same artifact across heph
/// processes so two runs don't fetch (or half-write) the same file at once.
#[cfg(unix)]
struct FileLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `self.file` owns a valid fd for the lifetime of this guard.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(unix)]
fn lock_exclusive(path: &std::path::Path) -> anyhow::Result<FileLock> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))?;
    // Blocking exclusive acquire; advisory and tied to the open file description.
    // SAFETY: `file` owns the fd; `flock` is valid for the call's duration.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("flock LOCK_EX on {}", path.display()));
    }
    Ok(FileLock { file })
}

/// The user-global plugin download cache: `~/.heph/plugins/<os>-<arch>/`. Plugins
/// are workspace-independent, so they're cached once per user (not per repo).
#[cfg(unix)]
fn plugin_cache_dir() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate ~/.heph plugin cache"))?;
    Ok(home.join(".heph").join("plugins").join(format!(
        "{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )))
}

/// Wipe this host's plugin download cache (`~/.heph/plugins/<os>-<arch>/`) so the
/// next resolve re-fetches. Used by `heph tool resolve-plugins --force`.
#[cfg(unix)]
pub fn clear_plugin_cache() -> anyhow::Result<()> {
    let dir = plugin_cache_dir()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clear plugin cache {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn clear_plugin_cache() -> anyhow::Result<()> {
    Ok(())
}

/// Cache-entry filename for a download URL: `<hash16>-<basename>`. Keyed by a
/// hash of the *full* URL (not just the basename) so a version bump — which
/// changes the URL (e.g. `.../v1.2.3/x.so` → `.../v1.2.4/x.so`) even when the
/// basename is identical — auto-invalidates instead of silently reusing the old
/// artifact. The basename is kept for readability + a correct extension.
fn cache_entry_name(url: &str) -> String {
    let basename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("plugin");
    let key = xxhash_rust::xxh3::xxh3_64(url.as_bytes());
    format!("{key:016x}-{basename}")
}

/// Download a plugin file from `url`, cache it under
/// `~/.heph/plugins/<os>-<arch>/`, make it executable, and return its path. A
/// previously-downloaded file is reused (no re-fetch). The URL is used verbatim —
/// the manifest carries fully-qualified per-os/arch artifact URLs.
#[cfg(unix)]
fn download_plugin(url: &str) -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let name = cache_entry_name(url);
    let dir = plugin_cache_dir()?;
    let dest = dir.join(&name);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    // Serialize concurrent downloads of this artifact (across processes too): hold
    // an exclusive lock for the fetch, then re-check — another run may have just
    // installed it while we waited.
    let _lock = lock_exclusive(&dir.join(format!("{name}.lock")))?;
    if dest.exists() {
        return Ok(dest);
    }

    // Download to a temp path then rename so a partial download is never seen as
    // a usable binary by a concurrent run.
    let tmp = dir.join(format!(".{name}.download"));
    if let Err(err) = fetch_to_file(url, &tmp, DOWNLOAD_READ_TIMEOUT) {
        // Best effort: a leftover is truncated by the next attempt anyway.
        if let Err(rm_err) = std::fs::remove_file(&tmp) {
            tracing::debug!(path = %tmp.display(), error = %rm_err, "remove partial plugin download");
        }
        return Err(err);
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", tmp.display()))?;
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("install plugin to {}", dest.display()))?;
    Ok(dest)
}

/// Bound on establishing the connection to a plugin download host.
#[cfg(unix)]
const DOWNLOAD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Bound on a single read making no progress. There is deliberately no bound on
/// the whole request: a plugin dylib is tens of MB, and a total timeout fails a
/// slow-but-progressing link on every attempt, while a stalled one still trips
/// this.
#[cfg(unix)]
const DOWNLOAD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// GET `url` and stream its body into `dest` (created or truncated). Fails if
/// the connection can't be made within [`DOWNLOAD_CONNECT_TIMEOUT`] or any read
/// waits longer than `read_timeout`; the total transfer time is unbounded.
///
/// Runs on a dedicated std thread with its own runtime so it is safe to call
/// from within the async runtime `new_engine` runs on (a nested `block_on`
/// would otherwise panic). The blocking client isn't used because it only
/// offers a total timeout, not a per-read one.
#[cfg(unix)]
fn fetch_to_file(
    url: &str,
    dest: &std::path::Path,
    read_timeout: std::time::Duration,
) -> anyhow::Result<()> {
    use std::io::Write;

    let url = url.to_string();
    let dest = dest.to_path_buf();
    std::thread::spawn(move || -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build plugin download runtime")?;
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
                .read_timeout(read_timeout)
                .build()
                .context("build plugin download client")?;
            let mut resp = client
                .get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .with_context(|| format!("GET {url}"))?;

            let f = std::fs::File::create(&dest)
                .with_context(|| format!("create {}", dest.display()))?;
            let mut f = std::io::BufWriter::new(f);
            while let Some(chunk) = resp
                .chunk()
                .await
                .with_context(|| format!("download {url}"))?
            {
                f.write_all(&chunk)
                    .with_context(|| format!("write {}", dest.display()))?;
            }
            f.flush()
                .with_context(|| format!("write {}", dest.display()))?;
            Ok(())
        })
    })
    .join()
    .map_err(|_e| anyhow::anyhow!("plugin download thread panicked"))?
}

#[cfg(test)]
mod tests {
    #[test]
    fn cache_entry_name_keys_on_full_url() {
        use super::cache_entry_name;
        let a = cache_entry_name("https://h/v1.2.3/heph-go-plugin_linux_amd64.so");
        let b = cache_entry_name("https://h/v1.2.4/heph-go-plugin_linux_amd64.so");
        // Same basename, different version → different cache entry (auto-invalidate).
        assert_ne!(a, b);
        assert!(a.ends_with("-heph-go-plugin_linux_amd64.so"), "{a}");
        // Stable for the same URL.
        assert_eq!(
            a,
            cache_entry_name("https://h/v1.2.3/heph-go-plugin_linux_amd64.so")
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_checksum_accepts_matching_sha256() {
        use super::verify_checksum;
        use sha2::Digest;
        let bytes = b"test";
        let hex = hex::encode(sha2::Sha256::digest(bytes));
        verify_checksum(bytes, &format!("sha256:{hex}")).expect("match");
        // case-insensitive algo tolerated.
        verify_checksum(bytes, &format!("SHA256:{hex}")).expect("match ci");
    }

    #[cfg(unix)]
    #[test]
    fn verify_checksum_rejects_mismatch() {
        use super::verify_checksum;
        let err = verify_checksum(b"test", "sha256:00").expect_err("mismatch");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn verify_checksum_rejects_unknown_algo() {
        use super::verify_checksum;
        let err = verify_checksum(b"x", "md5:abc").expect_err("bad algo");
        assert!(err.to_string().contains("unsupported"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn verify_checksum_rejects_malformed_spec() {
        use super::verify_checksum;
        // No `algo:hex` separator.
        let err = verify_checksum(b"x", "deadbeef").expect_err("no colon");
        assert!(err.to_string().contains("algo:hex"), "{err}");
        // Non-hex digest.
        let err = verify_checksum(b"x", "sha256:zz").expect_err("bad hex");
        assert!(err.to_string().contains("hex"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_path_expands_tilde_and_anchors_relatives() {
        use super::resolve_path;
        let base = std::path::Path::new("/base");
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .expect("HOME");
        // `~/` and bare `~` expand to $HOME (absolute → base ignored).
        assert_eq!(
            resolve_path("~/.heph/plugins/go.json", base),
            home.join(".heph/plugins/go.json")
        );
        assert_eq!(resolve_path("~", base), home);
        // Absolute stays as-is; `~user`/embedded `~` are not tilde-expanded.
        assert_eq!(
            resolve_path("/abs/path", base),
            std::path::PathBuf::from("/abs/path")
        );
        // Relative (incl. `~user`) anchors onto base.
        assert_eq!(resolve_path("rel/path", base), base.join("rel/path"));
        assert_eq!(resolve_path("~bob/x", base), base.join("~bob/x"));
    }

    /// Serve one HTTP response on a local port: headers announcing `len` body
    /// bytes, then `body` one byte every `gap`, then hold the connection open for
    /// `hold`. Returns the URL.
    #[cfg(unix)]
    fn serve_dribble(
        len: usize,
        body: &'static [u8],
        gap: std::time::Duration,
        hold: std::time::Duration,
    ) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).expect("read request");
            assert!(n > 0, "empty request");
            write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n"
            )
            .expect("write headers");
            for b in body {
                std::thread::sleep(gap);
                s.write_all(&[*b]).expect("write body");
                s.flush().expect("flush");
            }
            std::thread::sleep(hold);
        });
        format!("http://{addr}/heph-go-plugin_linux_amd64.so")
    }

    /// A transfer taking far longer in total than the read timeout still
    /// completes, as long as every read makes progress — the regression was a
    /// 30s bound on the whole request.
    #[cfg(unix)]
    #[test]
    fn fetch_to_file_tolerates_slow_but_progressing_body() {
        use std::time::Duration;
        let body: &[u8] = b"0123456789";
        let url = serve_dribble(body.len(), body, Duration::from_millis(100), Duration::ZERO);
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("out");
        // ~1s total, 400ms per read.
        super::fetch_to_file(&url, &dest, Duration::from_millis(400)).expect("download");
        assert_eq!(std::fs::read(&dest).expect("read"), body);
    }

    /// A body that stops arriving fails on the read timeout, and the error
    /// names the artifact.
    #[cfg(unix)]
    #[test]
    fn fetch_to_file_fails_on_stalled_body_naming_the_url() {
        use std::time::Duration;
        let url = serve_dribble(10, b"01", Duration::ZERO, Duration::from_secs(5));
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("out");
        let err = super::fetch_to_file(&url, &dest, Duration::from_millis(200))
            .expect_err("stalled body must time out");
        let msg = format!("{err:#}");
        assert!(msg.contains(&url), "{msg}");
    }
}
