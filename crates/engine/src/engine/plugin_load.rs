//! Resolving the `plugins:` config into a live registry: applying built-ins,
//! and resolving + downloading + loading cdylib plugins from their
//! `*-plugin.json` manifests. The bin builds the engine and registers the
//! in-process plugin factories; the engine owns turning config into loaded
//! plugins (here), including the download cache under `~/.heph/plugins`.

use anyhow::Context;

use super::Engine;
use super::config_yaml;

impl Engine {
    /// Apply every `plugins:` entry. `builtin:` entries instantiate the matching
    /// in-process factory (registered before this call); `path:`/`url:` entries
    /// resolve a `*-plugin.json` manifest, load the cdylib it names for this
    /// host, and register the provider + drivers it exports (the plugin
    /// self-describes its names via `config()`).
    pub fn register_plugins(&mut self, plugins: &[config_yaml::PluginSpec]) -> anyhow::Result<()> {
        let mut manifests: Vec<(config_yaml::PluginIdentifier, Vec<u8>)> = Vec::new();
        for spec in plugins {
            match &spec.identifier {
                config_yaml::PluginIdentifier::Builtin(name) => self
                    .apply_builtin(name, &spec.options)
                    .with_context(|| format!("apply builtin plugin `{name}`"))?,
                // Manifest plugin: encode options once (pb::Value bytes) for the
                // cdylib's create entry, resolve + load below.
                _ => manifests.push((
                    spec.identifier.clone(),
                    hplugin_abi::convert::options_to_pb_bytes(&spec.options),
                )),
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
}

/// Resolve each manifest to a host dylib, load it (parallel — loading constructs
/// the plugin, which isn't free), and register its exported components directly.
#[cfg(unix)]
fn load_dylib_plugins(
    e: &mut Engine,
    root: &std::path::Path,
    home_dir: &std::path::Path,
    manifests: Vec<(config_yaml::PluginIdentifier, Vec<u8>)>,
) -> anyhow::Result<()> {
    use rayon::prelude::*;

    if manifests.is_empty() {
        return Ok(());
    }
    let root_str = root.to_string_lossy().into_owned();
    let home_str = home_dir.to_string_lossy().into_owned();
    let loaded = manifests
        .into_par_iter()
        .map(|(src, options_pb)| -> anyhow::Result<_> {
            let dylib = resolve_manifest_dylib(&src, root)?;
            hplugin_stabby::load_stable::load(&dylib, &root_str, &home_str, &options_pb)
                .with_context(|| format!("load plugin dylib {}", dylib.display()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Engine mutation is single-threaded; register the loaded components in order.
    for (provider, drivers) in loaded {
        if let Some(p) = provider {
            e.register_provider(|_| Box::new(p))?;
        }
        for (_name, drv) in drivers {
            e.register_managed_driver(|_| Box::new(drv))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn load_dylib_plugins(
    _e: &mut Engine,
    _root: &std::path::Path,
    _home_dir: &std::path::Path,
    manifests: Vec<(config_yaml::PluginIdentifier, Vec<u8>)>,
) -> anyhow::Result<()> {
    if manifests.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("`path:`/`url:` (cdylib) plugins are only supported on unix")
    }
}

/// Expand a leading `~` / `~/` to `$HOME` so config and manifest paths can point
/// at the user-global plugin dir (`~/.heph/plugins/...`). A bare `~` or `~/rest`
/// expands; `~user` and embedded `~` are left untouched. Returns the input
/// unchanged when `HOME` is unset.
#[cfg(unix)]
fn expand_tilde(p: &str) -> std::path::PathBuf {
    let rest = if p == "~" {
        Some("")
    } else {
        p.strip_prefix("~/")
    };
    match (rest, std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(p),
    }
}

/// Resolve a plugin manifest source to the concrete dylib path for this host:
/// load the manifest (local read or download), pick the artifact matching the
/// host os/arch, then resolve that artifact (a local `path` sibling of the
/// manifest, or a `url` to download + cache).
#[cfg(unix)]
fn resolve_manifest_dylib(
    src: &config_yaml::PluginIdentifier,
    root: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let (manifest_bytes, manifest_dir) = match src {
        config_yaml::PluginIdentifier::Path(p) => {
            let pb = expand_tilde(p);
            let mp = if pb.is_absolute() { pb } else { root.join(pb) };
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
    match (&art.path, &art.url) {
        (Some(p), None) => {
            let pb = expand_tilde(p);
            Ok(if pb.is_absolute() {
                pb
            } else {
                manifest_dir.join(pb)
            })
        }
        (None, Some(u)) => download_plugin(u),
        _ => anyhow::bail!(
            "plugin `{}` artifact for {os}/{arch} must set exactly one of `path`/`url`",
            manifest.name
        ),
    }
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

/// Substitute `{os}`/`{arch}` in a plugin download URL with the values used in
/// published artifact names: os `linux`/`darwin` and arch `amd64`/`arm64`
/// (mapped from the rust `std::env::consts` spellings `macos` and
/// `x86_64`/`aarch64`). Unmapped hosts fall back to the raw consts value.
fn substitute_os_arch(url: &str) -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    url.replace("{os}", os).replace("{arch}", arch)
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

/// Download a plugin file from `url_tmpl` (after `{os}`/`{arch}` substitution),
/// cache it under `~/.heph/plugins/<os>-<arch>/`, make it executable, and return
/// its path. A previously-downloaded file is reused (no re-fetch).
#[cfg(unix)]
fn download_plugin(url_tmpl: &str) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let url = substitute_os_arch(url_tmpl);
    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("plugin");
    let dir = plugin_cache_dir()?;
    let dest = dir.join(filename);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    // Serialize concurrent downloads of this artifact (across processes too): hold
    // an exclusive lock for the fetch, then re-check — another run may have just
    // installed it while we waited.
    let _lock = lock_exclusive(&dir.join(format!("{filename}.lock")))?;
    if dest.exists() {
        return Ok(dest);
    }

    // reqwest::blocking spins up its own runtime; run it on a dedicated std
    // thread so it is safe to call from within the async runtime new_engine runs
    // on (a nested block_on would otherwise panic).
    let url_for_thread = url.clone();
    let bytes = std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
        let resp = reqwest::blocking::get(&url_for_thread)
            .with_context(|| format!("GET {url_for_thread}"))?
            .error_for_status()
            .with_context(|| format!("GET {url_for_thread}"))?;
        Ok(resp.bytes()?.to_vec())
    })
    .join()
    .map_err(|_e| anyhow::anyhow!("plugin download thread panicked"))??;

    // Write to a temp path then rename so a partial download is never seen as a
    // usable binary by a concurrent run.
    let tmp = dir.join(format!(".{filename}.download"));
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&bytes)?;
        f.flush()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("install plugin to {}", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    #[test]
    fn substitute_os_arch_replaces_placeholders() {
        let out = super::substitute_os_arch("https://x/heph-{os}-{arch}.bin");
        assert!(!out.contains("{os}") && !out.contains("{arch}"), "{out}");
        // arch uses the published spelling (amd64/arm64), never the rust consts.
        assert!(!out.contains("x86_64") && !out.contains("aarch64"), "{out}");
        // os uses darwin, never the rust `macos` spelling.
        assert!(!out.contains("macos"), "{out}");
    }

    #[cfg(unix)]
    #[test]
    fn expand_tilde_expands_home_prefix_only() {
        use super::expand_tilde;
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .expect("HOME");
        assert_eq!(
            expand_tilde("~/.heph/plugins/go.json"),
            home.join(".heph/plugins/go.json")
        );
        assert_eq!(expand_tilde("~"), home);
        // No leading `~/`: left untouched (absolute, relative, and `~user`).
        assert_eq!(
            expand_tilde("/abs/path"),
            std::path::PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_tilde("rel/path"),
            std::path::PathBuf::from("rel/path")
        );
        assert_eq!(expand_tilde("~bob/x"), std::path::PathBuf::from("~bob/x"));
    }
}
