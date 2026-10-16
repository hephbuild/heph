//! Resolving the `plugins:` config into a live registry: applying built-ins,
//! and resolving + downloading + loading cdylib plugins from their
//! `*-plugin.json` manifests. The bin builds the engine and registers the
//! in-process plugin factories; the engine owns turning config into loaded
//! plugins (here), including the download cache under `~/.heph/plugins`.
//!
//! A cdylib artifact whose manifest declares a checksum is hashed once per
//! file version, not on every load: a stamp under
//! `~/.heph/plugins/<os>-<arch>/checksum-stamps/` records the file's identity
//! and change times beside its digest (see `ChecksumStamp`). To hash on every
//! load regardless, set `HEPH_VERIFY_PLUGINS=always`. `tool gc` drops the
//! stamps of plugins that no longer exist.

use anyhow::Context;

use super::Engine;
use super::config_yaml;

/// One resolved `path:`/`url:` plugin entry awaiting dylib load: its source, the
/// optional manifest checksum (`url:` only), and the options for the cdylib's
/// create entry.
struct ManifestPlugin {
    identifier: config_yaml::PluginIdentifier,
    checksum: Option<String>,
    options: config_yaml::Options,
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
                // Manifest plugin: carry the options to the cdylib's create entry,
                // resolve + load below.
                _ => manifests.push(ManifestPlugin {
                    identifier: spec.identifier.clone(),
                    checksum: spec.checksum.clone(),
                    options: spec.options.clone(),
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
    // Taken here, not probed inside `load`: the rayon workers below have no
    // current tokio runtime.
    let runtime = e.runtime.clone();
    // Each instance's registry handle resolves through this engine's slot, so a
    // second engine in the process never reaches these plugins' functions.
    let functions = e.function_slot();
    let loaded = manifests
        .into_par_iter()
        .map(|m| -> anyhow::Result<_> {
            let resolved = resolve_manifest_dylib(&m.identifier, m.checksum.as_deref(), root)?;
            let dylib = &resolved.dylib;
            // The plugin's name is its manifest's: the binary reports none.
            let hplugin_stabby::load_stable::LoadedComponents {
                provider,
                drivers,
                functions,
                hooks,
                runners,
            } = hplugin_stabby::load_stable::load(
                dylib,
                &hplugin_abi::convert::CreateConfig::new(
                    root,
                    home_dir.to_path_buf(),
                    resolved.name.clone(),
                    m.options,
                ),
                runtime.clone(),
                std::sync::Arc::clone(&functions),
            )
            .with_context(|| format!("load plugin dylib {}", dylib.display()))?;
            let mut parts = super::PluginParts {
                provider: provider
                    .map(|p| Box::new(p) as Box<dyn crate::engine::provider::Provider>),
                functions,
                hooks: hooks
                    .into_iter()
                    .map(|(_, h)| {
                        std::sync::Arc::new(h) as std::sync::Arc<dyn crate::engine::hook::Hook>
                    })
                    .collect(),
                runners,
                ..Default::default()
            };
            for (_local, drv) in drivers {
                parts = parts.with_managed_driver(Box::new(drv));
            }
            let source = format!(
                "cdylib plugin {} (manifest {})",
                dylib.display(),
                resolved.manifest
            );
            Ok((resolved.name, source, parts))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Engine mutation is single-threaded; register the loaded plugins in order.
    for (name, source, parts) in loaded {
        e.insert_plugin(&name, source, parts)?;
    }
    Ok(())
}

/// A manifest resolved to this host's artifact: the plugin's name (the
/// manifest's `name`, already checked against the naming rule), where the
/// manifest came from, and the dylib to load.
#[cfg(unix)]
struct ResolvedManifest {
    name: String,
    manifest: String,
    dylib: std::path::PathBuf,
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
) -> anyhow::Result<ResolvedManifest> {
    let (manifest_bytes, manifest_dir, manifest_src) = match src {
        config_yaml::PluginIdentifier::Path(p) => {
            let mp = resolve_path(p, root);
            let bytes = std::fs::read(&mp)
                .with_context(|| format!("read plugin manifest {}", mp.display()))?;
            let dir = mp
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| root.to_path_buf());
            (bytes, dir, mp.display().to_string())
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
            (bytes, dir, u.clone())
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
    // The manifest's `name` is the plugin's `heph.<name>` namespace and the
    // prefix of every provider/driver it ships: check it before loading.
    super::engine::validate_plugin_name(&manifest.name)
        .with_context(|| format!("plugin manifest {manifest_src}"))?;
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
    // per-artifact checksum, if one is declared. Hashed once per file version,
    // not once per load: see `verify_artifact_checksum`.
    if let Some(expected) = art.checksum.as_deref() {
        verify_artifact_checksum(&manifest.name, &dylib, expected)
            .with_context(|| format!("verify checksum of plugin `{}`", manifest.name))?;
    }
    Ok(ResolvedManifest {
        name: manifest.name,
        manifest: manifest_src,
        dylib,
    })
}

/// Verify `bytes` against an `algo:hex` checksum spec (currently only
/// `sha256:<hex>`). Errors on an unknown/malformed spec or a digest mismatch.
#[cfg(unix)]
fn verify_checksum(bytes: &[u8], expected: &str) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    let want = parse_checksum(expected)?;
    let got = Sha256::digest(bytes);
    if got.as_slice() != want.as_slice() {
        anyhow::bail!(
            "checksum mismatch: expected sha256:{}, got sha256:{}",
            hex::encode(&want),
            hex::encode(got)
        );
    }
    Ok(())
}

/// The digest bytes of an `algo:hex` checksum spec (only `sha256`). Errors on an
/// unknown algorithm or malformed hex.
#[cfg(unix)]
fn parse_checksum(expected: &str) -> anyhow::Result<Vec<u8>> {
    let (algo, want_hex) = expected
        .split_once(':')
        .with_context(|| format!("checksum must be `algo:hex`, got {expected:?}"))?;
    if !algo.eq_ignore_ascii_case("sha256") {
        anyhow::bail!("unsupported checksum algorithm {algo:?} (only sha256)");
    }
    hex::decode(want_hex).with_context(|| format!("checksum hex is not valid hex: {want_hex:?}"))
}

/// How [`verify_artifact_checksum_in`] established that an artifact matches.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum ArtifactCheck {
    /// A stamp recorded this exact file version as matching; nothing was read.
    Stamped,
    /// The file was read and hashed (and matched).
    Hashed,
}

/// Version of the [`ChecksumStamp`] on-disk format. A stamp with any other
/// version — or one that fails to parse — is a miss: the artifact is re-hashed
/// and the stamp rewritten.
#[cfg(unix)]
const CHECKSUM_STAMP_VERSION: u32 = 1;

/// The record that one version of a plugin artifact was hashed and matched.
///
/// A plugin cdylib is tens of megabytes, and reading and hashing it on every
/// load was most of the cost of loading it. After a successful verification
/// the file's identity and change metadata are recorded next to its digest;
/// a later load whose expected digest equals the stamp's and whose `stat`
/// matches every field skips the hash.
///
/// The security property is `ctime`. Any write to the file — and any
/// `chmod`/`chown`/`utimes`, which is how an attacker would put back an old
/// `mtime` — sets the inode change time to *now*, and no unprivileged process
/// can set it back. So a file that was modified after it was stamped cannot
/// present the stamped `ctime`, and is re-hashed. `dev` + `ino` pin the stamp to
/// one inode (a file swapped in by rename is a different inode), and `size` and
/// `mtime` are cheap extra discriminators. What this does not defend against is
/// what hashing on every load did not defend against either: root, or a write
/// to the stamp *and* the file by someone who can already write the user's
/// `~/.heph` (who could as well swap the plugin cache itself).
///
/// The trade-off: corruption that leaves the metadata alone — bit rot, or a
/// write by root that put `ctime` back — is no longer caught on every load. It
/// is caught when the metadata next changes, or on any load under
/// [`VERIFY_PLUGINS_ENV`]`=always`.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ChecksumStamp {
    v: u32,
    /// The artifact's path, as opened. The stamp's file name is a hash of it;
    /// recorded so `tool gc` can drop the stamps of plugins that are gone.
    path: String,
    #[serde(flatten)]
    file: FileVersion,
    /// Lowercase hex sha256 of the file's content.
    sha256: String,
}

/// The `stat` fields that identify one version of a file: which inode, and
/// when it last changed.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FileVersion {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
}

#[cfg(unix)]
impl FileVersion {
    /// `None` for a timestamp outside `i64` nanoseconds — such a file is simply
    /// never stamped.
    fn of(meta: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt as _;
        let ns = |sec: i64, nsec: i64| sec.checked_mul(1_000_000_000)?.checked_add(nsec);
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec())?,
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec())?,
        })
    }
}

#[cfg(unix)]
impl ChecksumStamp {
    /// The stamp for the file at `path` with metadata `meta` and digest
    /// `sha256`. `None` for a path that is not UTF-8 or a timestamp out of
    /// range — such a file is simply never stamped.
    fn of(path: &std::path::Path, meta: &std::fs::Metadata, sha256: String) -> Option<Self> {
        Some(Self {
            v: CHECKSUM_STAMP_VERSION,
            path: path.to_str()?.to_owned(),
            file: FileVersion::of(meta)?,
            sha256,
        })
    }

    fn read(path: &std::path::Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let stamp: Self = serde_json::from_slice(&bytes).ok()?;
        (stamp.v == CHECKSUM_STAMP_VERSION).then_some(stamp)
    }

    /// Write atomically (temp file + rename in the same directory), so a
    /// concurrent reader sees the old stamp, the new one, or none — never half.
    fn write(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write as _;
        let dir = path
            .parent()
            .context("checksum stamp path has no parent directory")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating checksum stamp dir {}", dir.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("creating temp checksum stamp in {}", dir.display()))?;
        tmp.write_all(&serde_json::to_vec(self).context("encoding checksum stamp")?)
            .context("writing temp checksum stamp")?;
        tmp.persist(path)
            .with_context(|| format!("installing checksum stamp {}", path.display()))?;
        Ok(())
    }
}

/// Where the stamp for the artifact at `path` lives inside `stamp_dir`: named
/// by a hash of the path, so downloaded and local-path plugins share one
/// directory and a read-only plugin location (a nix store path) needs no write
/// access.
#[cfg(unix)]
fn checksum_stamp_path(stamp_dir: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt as _;
    let key = hex::encode(Sha256::digest(path.as_os_str().as_bytes()));
    stamp_dir.join(format!("{key}.json"))
}

/// The stamp directory's name under the plugin cache dir.
#[cfg(unix)]
const CHECKSUM_STAMP_DIR: &str = "checksum-stamps";

/// Set `HEPH_VERIFY_PLUGINS=always` to hash every checksummed plugin artifact
/// on every load, trusting no [`ChecksumStamp`] (a successful hash still
/// refreshes the stamp). For when corruption that file metadata cannot show is
/// suspected: bit rot, or a write by root that put `ctime` back. Any other
/// non-empty value is ignored with a warning.
#[cfg(unix)]
const VERIFY_PLUGINS_ENV: &str = "HEPH_VERIFY_PLUGINS";

/// Whether [`VERIFY_PLUGINS_ENV`] asks for every load to hash.
#[cfg(unix)]
fn verify_plugins_always() -> bool {
    let Some(value) = std::env::var_os(VERIFY_PLUGINS_ENV) else {
        return false;
    };
    if value == "always" {
        return true;
    }
    if !value.is_empty() {
        tracing::warn!(
            var = VERIFY_PLUGINS_ENV,
            value = %value.to_string_lossy(),
            "unrecognized value (only `always` is); plugin checksum stamps stay trusted"
        );
    }
    false
}

/// No stamp is written for a file whose `ctime` or `mtime` is this close to
/// now, or ahead of it — git's racy-index rule. A filesystem with coarse
/// timestamps (whole seconds; two on FAT) gives a write landing in the same
/// granule as the stamp's snapshot the very same times, so the stamp would
/// still match the changed file. Once the file's times are older than the
/// coarsest granule, any later write moves `ctime`. The next load hashes again
/// and stamps then.
#[cfg(unix)]
const RACY_STAMP_WINDOW_NS: i128 = 2_000_000_000;

/// How one verification consults and keeps its [`ChecksumStamp`].
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct StampPolicy<'a> {
    /// Where stamps live; `None` (no `HOME`) means none are read or written.
    dir: Option<&'a std::path::Path>,
    /// Hash even when a stamp matches ([`VERIFY_PLUGINS_ENV`]`=always`).
    force: bool,
    /// The clock the racy-stamp rule reads, once the file has been hashed.
    now: fn() -> std::time::SystemTime,
}

/// Verify the artifact of plugin `plugin` at `path` against `expected`
/// (`algo:hex`), trusting a [`ChecksumStamp`] under the plugin cache dir when
/// it matches. A missing `HOME` just means no stamps: every load hashes, as
/// before.
#[cfg(unix)]
fn verify_artifact_checksum(
    plugin: &str,
    path: &std::path::Path,
    expected: &str,
) -> anyhow::Result<()> {
    let stamp_dir = plugin_cache_dir().ok().map(|d| d.join(CHECKSUM_STAMP_DIR));
    let policy = StampPolicy {
        dir: stamp_dir.as_deref(),
        force: verify_plugins_always(),
        now: std::time::SystemTime::now,
    };
    verify_artifact_checksum_in(plugin, path, expected, policy, sha256_hex).map(drop)
}

/// The lowercase hex sha256 of everything left to read from `file`.
#[cfg(unix)]
fn sha256_hex(file: &mut std::fs::File) -> std::io::Result<String> {
    struct Hasher(sha2::Sha256);
    impl std::io::Write for Hasher {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            sha2::Digest::update(&mut self.0, buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hasher = Hasher(sha2::Digest::new());
    std::io::copy(
        &mut std::io::BufReader::with_capacity(256 * 1024, file),
        &mut hasher,
    )?;
    Ok(hex::encode(sha2::Digest::finalize(hasher.0)))
}

/// [`verify_artifact_checksum`] with its stamp policy and its hash function
/// passed in, so a test can control the clock and act between the reads.
#[cfg(unix)]
fn verify_artifact_checksum_in(
    plugin: &str,
    path: &std::path::Path,
    expected: &str,
    policy: StampPolicy<'_>,
    hash: impl FnOnce(&mut std::fs::File) -> std::io::Result<String>,
) -> anyhow::Result<ArtifactCheck> {
    // The spec is validated on every load, stamp or not.
    let want_hex = hex::encode(parse_checksum(expected)?);
    // One descriptor for everything: the metadata compared — against the stamp
    // before, against itself after — is that of the very file whose bytes are
    // hashed, even if something is renamed over `path` meanwhile.
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open plugin artifact {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("stat plugin artifact {}", path.display()))?;
    let current = ChecksumStamp::of(path, &meta, want_hex.clone());
    let stamp_path = policy.dir.map(|d| checksum_stamp_path(d, path));
    let reason = match &stamp_path {
        _ if policy.force => "forced",
        None => "no stamp dir",
        Some(sp) => match ChecksumStamp::read(sp) {
            None => "no stamp",
            Some(stamp) if stamp.sha256 != want_hex => "digest changed",
            Some(stamp) if Some(&stamp) == current.as_ref() => {
                tracing::debug!(plugin, path = %path.display(), "checksum trusted from stamp");
                return Ok(ArtifactCheck::Stamped);
            }
            Some(_) => "metadata changed",
        },
    };
    tracing::debug!(plugin, path = %path.display(), reason, "hashing plugin artifact");

    let got_hex = hash(&mut file)
        .with_context(|| format!("read plugin artifact {} for checksum", path.display()))?;
    if got_hex != want_hex {
        anyhow::bail!(
            "checksum mismatch for {}: expected sha256:{want_hex}, got sha256:{got_hex}; \
             re-download or remove the plugin, then retry",
            path.display()
        );
    }

    if let (Some(sp), Some(stamp)) = (&stamp_path, current) {
        let not_written = match stamp_refusal(&file, &stamp.file, policy.now) {
            Some(why) => Some(why.to_owned()),
            None => stamp.write(sp).err().map(|e| format!("{e:#}")),
        };
        if let Some(why) = not_written {
            tracing::debug!(plugin, path = %path.display(), reason = %why, "plugin checksum stamp not written");
        }
    }
    Ok(ArtifactCheck::Hashed)
}

/// Why the stamp of `version` must not be written for the just-hashed `file`,
/// if it must not.
#[cfg(unix)]
fn stamp_refusal(
    file: &std::fs::File,
    version: &FileVersion,
    now: fn() -> std::time::SystemTime,
) -> Option<&'static str> {
    // The digest is of the bytes read, and they belong to the stamped version
    // only if it did not change while they were being read.
    let after = file.metadata().ok();
    if after.as_ref().and_then(FileVersion::of).as_ref() != Some(version) {
        return Some("changed while hashed");
    }
    // Read the clock only now, after the last look at the file: a write after
    // this instant gets a `ctime` the racy rule guarantees differs.
    let now_ns = now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX));
    let newest = i128::from(version.ctime_ns.max(version.mtime_ns));
    (newest > now_ns - RACY_STAMP_WINDOW_NS).then_some("racy")
}

/// Remove the checksum stamps whose plugin file no longer exists, and those
/// that no longer parse (a miss on every load anyway). Returns how many were
/// removed. No `HOME` or no stamp directory removes nothing.
#[cfg(unix)]
pub fn sweep_checksum_stamps() -> anyhow::Result<usize> {
    match plugin_cache_dir() {
        Ok(dir) => sweep_checksum_stamps_in(&dir.join(CHECKSUM_STAMP_DIR)),
        Err(_) => Ok(0),
    }
}

#[cfg(not(unix))]
pub fn sweep_checksum_stamps() -> anyhow::Result<usize> {
    Ok(0)
}

#[cfg(unix)]
fn sweep_checksum_stamps_in(dir: &std::path::Path) -> anyhow::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e).with_context(|| format!("listing checksum stamps in {}", dir.display()));
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("listing checksum stamps in {}", dir.display()))?;
        let path = entry.path();
        // Anything else is a temp file of a write in flight.
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        // An error (say, permission) is not proof the plugin is gone: keep it.
        let stale = ChecksumStamp::read(&path).is_none_or(|stamp| {
            std::path::Path::new(&stamp.path)
                .try_exists()
                .is_ok_and(|exists| !exists)
        });
        if !stale {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "removing stale plugin checksum stamp");
            }
        }
    }
    Ok(removed)
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
    if let Err(err) = download_to(url, &tmp) {
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

/// Stream `url` into `dest` (created or truncated).
#[cfg(unix)]
fn download_to(url: &str, dest: &std::path::Path) -> anyhow::Result<()> {
    let f = std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    hfetch::get(url, &mut std::io::BufWriter::new(f))
        .with_context(|| format!("download plugin to {}", dest.display()))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod checksum_stamp_tests {
    use super::{
        ArtifactCheck, ChecksumStamp, StampPolicy, checksum_stamp_path, sha256_hex,
        sweep_checksum_stamps_in, verify_artifact_checksum_in,
    };
    use sha2::Digest;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn spec(content: &[u8]) -> String {
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(content)))
    }

    /// A clock an hour ahead, so a fixture file written just now is old enough
    /// to stamp. The racy rule has its own tests, on the real clock.
    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(3600)
    }

    fn policy(stamps: Option<&Path>) -> StampPolicy<'_> {
        StampPolicy {
            dir: stamps,
            force: false,
            now: later,
        }
    }

    fn verify(
        path: &Path,
        expected: &str,
        policy: StampPolicy<'_>,
    ) -> anyhow::Result<ArtifactCheck> {
        verify_artifact_checksum_in("test", path, expected, policy, sha256_hex)
    }

    fn check(path: &Path, expected: &str, stamps: &Path) -> ArtifactCheck {
        verify(path, expected, policy(Some(stamps))).expect("verifies")
    }

    fn set_mtime(path: &Path, to: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(to))
            .expect("set mtime");
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        plugin: std::path::PathBuf,
        stamps: std::path::PathBuf,
    }

    fn fixture(content: &[u8]) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin = dir.path().join("plugin.dylib");
        std::fs::write(&plugin, content).expect("write plugin");
        let stamps = dir.path().join("stamps");
        Fixture {
            _dir: dir,
            plugin,
            stamps,
        }
    }

    /// First load hashes and stamps; the next load of the same file trusts the
    /// stamp and reads nothing.
    #[test]
    fn a_matching_stamp_skips_hashing() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
        assert!(checksum_stamp_path(&f.stamps, &f.plugin).exists());
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Stamped);
    }

    /// Any metadata change is a miss: here only `mtime` moves.
    #[test]
    fn a_metadata_change_rehashes() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        check(&f.plugin, &want, &f.stamps);
        let file = std::fs::File::options()
            .write(true)
            .open(&f.plugin)
            .expect("open");
        file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7))
            .expect("set mtime");
        drop(file);
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Stamped);
    }

    /// The ctime property: rewriting the file in place with the very same
    /// content, same size, and its original mtime put back, still misses —
    /// the write moved ctime, which nobody unprivileged can restore.
    #[test]
    fn an_identical_rewrite_with_restored_mtime_still_rehashes() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        check(&f.plugin, &want, &f.stamps);
        let mtime = std::fs::metadata(&f.plugin)
            .and_then(|m| m.modified())
            .expect("mtime");
        std::fs::write(&f.plugin, b"plugin bytes").expect("rewrite");
        let file = std::fs::File::options()
            .write(true)
            .open(&f.plugin)
            .expect("open");
        file.set_modified(mtime).expect("restore mtime");
        drop(file);
        assert_eq!(
            std::fs::metadata(&f.plugin)
                .and_then(|m| m.modified())
                .expect("mtime"),
            mtime,
            "fixture: mtime is restored"
        );
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
    }

    /// A stamp that does not parse, or carries another format version, is a
    /// miss — and the re-hash replaces it with a good one.
    #[test]
    fn a_corrupt_or_unknown_version_stamp_rehashes() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        check(&f.plugin, &want, &f.stamps);
        let stamp = checksum_stamp_path(&f.stamps, &f.plugin);

        std::fs::write(&stamp, b"{not json").expect("corrupt");
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Stamped);

        let text = std::fs::read_to_string(&stamp).expect("read stamp");
        std::fs::write(&stamp, text.replace("\"v\":1", "\"v\":99")).expect("bump");
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
    }

    /// A mismatch still fails, stamp or not: a different expected digest never
    /// matches the stamp, and a changed file is re-hashed and refused — without
    /// its stamp being updated to the bad content.
    #[test]
    fn a_checksum_mismatch_still_fails() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        check(&f.plugin, &want, &f.stamps);

        let other = spec(b"other bytes");
        let err = verify(&f.plugin, &other, policy(Some(&f.stamps)))
            .expect_err("a stamp for another digest must not satisfy this one");
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");

        std::fs::write(&f.plugin, b"tampered bytes").expect("tamper");
        let err = verify(&f.plugin, &want, policy(Some(&f.stamps)))
            .expect_err("tampered content must fail");
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        let err = verify(&f.plugin, &want, policy(Some(&f.stamps)))
            .expect_err("and keep failing: the failure wrote no stamp");
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
    }

    /// The mismatch error names the artifact and says what to do about it.
    #[test]
    fn a_checksum_mismatch_names_the_file_and_the_fix() {
        let f = fixture(b"plugin bytes");
        let err = verify(&f.plugin, &spec(b"other bytes"), policy(Some(&f.stamps)))
            .expect_err("mismatch");
        let msg = format!("{err:#}");
        assert!(msg.contains(&f.plugin.display().to_string()), "{msg}");
        assert!(
            msg.contains("re-download or remove the plugin, then retry"),
            "{msg}"
        );
    }

    /// Git's racy rule: a file whose ctime or mtime is within the window of
    /// now (or ahead of it) is hashed but not stamped; the next load, once the
    /// file is old enough, stamps it.
    #[test]
    fn a_racy_file_is_hashed_but_not_stamped() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        let stamp = checksum_stamp_path(&f.stamps, &f.plugin);
        let real_clock = StampPolicy {
            now: SystemTime::now,
            ..policy(Some(&f.stamps))
        };

        // Just written: ctime is now.
        assert_eq!(
            verify(&f.plugin, &want, real_clock).expect("verifies"),
            ArtifactCheck::Hashed
        );
        assert!(
            !stamp.exists(),
            "a file changed just now must not be stamped"
        );

        // Old enough by ctime, but mtime is in the future (past the clock).
        set_mtime(&f.plugin, later() + Duration::from_secs(3600));
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
        assert!(!stamp.exists(), "a future mtime must not be stamped");

        set_mtime(&f.plugin, SystemTime::UNIX_EPOCH + Duration::from_secs(7));
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
        assert!(stamp.exists(), "an old enough file is stamped");
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Stamped);
    }

    /// A file whose metadata moves between the hash's two looks at it is
    /// hashed (and verified) but not stamped: the digest may be of neither
    /// version.
    #[test]
    fn a_file_changed_while_hashed_is_not_stamped() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        let outcome = verify_artifact_checksum_in(
            "test",
            &f.plugin,
            &want,
            policy(Some(&f.stamps)),
            |file| {
                let digest = sha256_hex(file);
                set_mtime(&f.plugin, SystemTime::UNIX_EPOCH + Duration::from_secs(7));
                digest
            },
        )
        .expect("verifies");
        assert_eq!(outcome, ArtifactCheck::Hashed);
        assert!(!checksum_stamp_path(&f.stamps, &f.plugin).exists());
        // Untouched since, it is stamped on the next load.
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Stamped);
    }

    /// The bytes hashed and the metadata stamped come from one descriptor. A
    /// different file renamed over the path after it was opened neither
    /// breaks this load (the descriptor still reads the verified file) nor is
    /// credited with its stamp: the stamp names the original inode, so the
    /// swapped-in file is hashed — and refused — on the next load.
    #[test]
    fn the_stamp_records_the_descriptor_that_was_hashed() {
        use std::os::unix::fs::MetadataExt as _;
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        let original_ino = std::fs::metadata(&f.plugin).expect("stat").ino();
        let swapped = f.plugin.with_extension("swapped");
        std::fs::write(&swapped, b"evil bytes").expect("write");

        let outcome = verify_artifact_checksum_in(
            "test",
            &f.plugin,
            &want,
            policy(Some(&f.stamps)),
            |file| {
                std::fs::rename(&swapped, &f.plugin).expect("swap");
                sha256_hex(file)
            },
        )
        .expect("the opened file is the verified one");
        assert_eq!(outcome, ArtifactCheck::Hashed);
        // Unlinking the original moves its ctime, so no stamp is the usual
        // outcome; one that was written can only name the inode hashed.
        if let Some(stamp) = ChecksumStamp::read(&checksum_stamp_path(&f.stamps, &f.plugin)) {
            assert_eq!(stamp.file.ino, original_ino);
        }

        let err = verify(&f.plugin, &want, policy(Some(&f.stamps)))
            .expect_err("the swapped-in file is hashed, not trusted");
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
    }

    /// A stamp that cannot be written (the stamp dir's path is a regular file,
    /// so this holds for root too) is not an error: the load verifies by
    /// hashing, every time.
    #[test]
    fn an_unwritable_stamp_dir_still_verifies() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        std::fs::write(&f.stamps, b"not a directory").expect("block the stamp dir");
        for _ in 0..2 {
            assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
        }
    }

    /// `HEPH_VERIFY_PLUGINS=always` hashes despite a matching stamp, and keeps
    /// the stamp fresh for loads without it.
    #[test]
    fn a_forced_verification_hashes_despite_a_stamp() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        let forced = StampPolicy {
            force: true,
            ..policy(Some(&f.stamps))
        };
        check(&f.plugin, &want, &f.stamps);
        for _ in 0..2 {
            assert_eq!(
                verify(&f.plugin, &want, forced).expect("verifies"),
                ArtifactCheck::Hashed
            );
        }
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Stamped);
    }

    /// `tool gc` drops the stamps of plugins that no longer exist, and stamps
    /// that no longer parse; it keeps the stamps of present plugins and leaves
    /// a write in flight (a temp file) alone.
    #[test]
    fn sweep_removes_stamps_of_missing_plugins() {
        let kept = fixture(b"kept");
        let stamps = kept.stamps.clone();
        let gone = kept.plugin.with_file_name("gone.dylib");
        std::fs::write(&gone, b"gone").expect("write");
        check(&kept.plugin, &spec(b"kept"), &stamps);
        check(&gone, &spec(b"gone"), &stamps);
        std::fs::remove_file(&gone).expect("remove plugin");
        let corrupt = stamps.join("corrupt.json");
        std::fs::write(&corrupt, b"{not json").expect("write");
        let in_flight = stamps.join(".tmpABCDEF");
        std::fs::write(&in_flight, b"{").expect("write");

        assert_eq!(sweep_checksum_stamps_in(&stamps).expect("sweeps"), 2);
        assert!(checksum_stamp_path(&stamps, &kept.plugin).exists());
        assert!(!checksum_stamp_path(&stamps, &gone).exists());
        assert!(!corrupt.exists());
        assert!(in_flight.exists());
        assert_eq!(
            check(&kept.plugin, &spec(b"kept"), &stamps),
            ArtifactCheck::Stamped
        );

        let missing = kept.plugin.with_file_name("no-stamps-here");
        assert_eq!(sweep_checksum_stamps_in(&missing).expect("sweeps"), 0);
    }

    /// A file swapped in by rename (what a download does) is a new inode: miss.
    #[test]
    fn a_replaced_file_rehashes() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        check(&f.plugin, &want, &f.stamps);
        let tmp = f.plugin.with_extension("new");
        std::fs::write(&tmp, b"plugin bytes").expect("write");
        std::fs::rename(&tmp, &f.plugin).expect("rename");
        assert_eq!(check(&f.plugin, &want, &f.stamps), ArtifactCheck::Hashed);
    }

    /// Without a stamp dir (no `HOME`) every load hashes, as before.
    #[test]
    fn no_stamp_dir_always_hashes() {
        let f = fixture(b"plugin bytes");
        let want = spec(b"plugin bytes");
        for _ in 0..2 {
            assert_eq!(
                verify(&f.plugin, &want, policy(None)).expect("verifies"),
                ArtifactCheck::Hashed
            );
        }
    }
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
}
