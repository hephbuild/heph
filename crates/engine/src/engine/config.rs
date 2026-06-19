//! The engine's **resolved** runtime configuration.
//!
//! This is the counterpart to [`ConfigYaml`](crate::engine::config_yaml::ConfigYaml):
//! where the YAML layer is all-optional and built up by layering profiles, this
//! layer has *no* optional fields — every default is applied exactly once, in
//! [`ConfigYaml::resolve`]. Everything downstream of [`resolve`](ConfigYaml::resolve)
//! sees a fully-populated [`Config`] and never has to reason about defaults.

use std::path::{Path, PathBuf};

use crate::engine::RemoteCacheDef;
use crate::engine::config_yaml::{ConfigYaml, FuseConfig, LockBackendConfig, MemCacheConfig};
use crate::engine::result_lock::LockBackend;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub root: PathBuf,
    /// Workspace state/cache directory. If empty, defaults to `root/.heph3`.
    pub home_dir: PathBuf,
    /// Repo-root-relative directories from the config file's `fs.skip`, pruned by
    /// every plugin that walks the tree. See [`Engine::skip_dirs`].
    ///
    /// [`Engine::skip_dirs`]: crate::engine::Engine::skip_dirs
    pub fs_skip: Vec<String>,
    pub parallelism: Option<usize>,
    /// In-memory tier fronting the durable (SQLite) local cache.
    pub mem_cache: MemCacheOptions,
    /// Mem-only store for tmp/uncacheable revisions ([`LocalCacheTmp`]).
    /// Entries over `per_entry_bytes`, or that would push the store past
    /// `capacity_bytes`, spill to the durable cache.
    ///
    /// [`LocalCacheTmp`]: crate::engine::local_cache_tmp::LocalCacheTmp
    pub tmp_cache: MemCacheOptions,
    pub fuse: FuseConfig,
    /// Backend serializing the execute phase per addr. Defaults to `Fs`.
    pub lock_backend: LockBackend,
    /// Durable blobs strictly larger than this spill to plain files under
    /// `<home>/cache/blobs/` instead of being stored inline in the sqlite DB;
    /// manifests always stay in sqlite. Keeps the DB / WAL small and lets large
    /// artifacts stream from the filesystem. See [`DEFAULT_SPILL_THRESHOLD_BYTES`].
    pub spill_threshold_bytes: u64,
    /// Anonymous usage telemetry. Defaults to `true` (opt-out via config).
    pub telemetry_enabled: bool,
    /// Named remote (shared) caches from the config's `caches:` map. Empty
    /// disables the remote-cache layer entirely. See [`RemoteCacheSet`].
    ///
    /// [`RemoteCacheSet`]: crate::engine::RemoteCacheSet
    pub remote_caches: Vec<RemoteCacheDef>,
}

/// Default spill threshold: 8 MiB. Above a few MB the filesystem beats sqlite
/// blob storage on throughput and big blobs would bloat the single-file DB and
/// its WAL; below it, artifacts stay in sqlite where small indexed reads and the
/// mem tier win. Tunable via `cache.spillThresholdBytes`.
pub const DEFAULT_SPILL_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

impl Default for Config {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            home_dir: PathBuf::new(),
            fs_skip: Vec::new(),
            parallelism: None,
            mem_cache: MemCacheOptions::default(),
            tmp_cache: MemCacheOptions::default_tmp(),
            fuse: FuseConfig::default(),
            lock_backend: LockBackend::default(),
            spill_threshold_bytes: DEFAULT_SPILL_THRESHOLD_BYTES,
            telemetry_enabled: true,
            remote_caches: Vec::new(),
        }
    }
}

/// Byte limits for one in-memory cache store. Used for both the local-cache mem
/// tier (`mem_cache`) and the tmp store (`tmp_cache`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemCacheOptions {
    /// Per-entry size cap. For `mem_cache`, larger entries pass through
    /// uncached; for `tmp_cache`, larger entries spill to the durable cache.
    pub per_entry_bytes: usize,
    /// Total byte budget. For `mem_cache`, `0` disables the in-memory layer
    /// entirely; for `tmp_cache`, entries that would exceed it spill to durable.
    pub capacity_bytes: u64,
}

impl Default for MemCacheOptions {
    /// Defaults for the local-cache mem tier.
    fn default() -> Self {
        Self {
            per_entry_bytes: 16 * 1024,
            capacity_bytes: 64 * 1024 * 1024,
        }
    }
}

impl MemCacheOptions {
    /// Defaults for the tmp store: 1 MiB per entry, 64 MiB total budget.
    pub fn default_tmp() -> Self {
        Self {
            per_entry_bytes: 1024 * 1024,
            capacity_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Resolve a [`ConfigYaml`] into the engine's runtime [`Config`].
///
/// [`ConfigYaml`] lives in the engine-free `config` crate, so this resolution —
/// which produces engine-only types ([`Config`], [`LockBackend`],
/// [`RemoteCacheDef`]) — is exposed as an extension trait on the engine side
/// rather than an inherent method. Bring it into scope to call `cfg.resolve(..)`.
pub trait ConfigYamlExt {
    fn resolve(&self, root: &Path) -> anyhow::Result<Config>;
}

impl ConfigYamlExt for ConfigYaml {
    /// Resolve this optional, profile-layered YAML into the engine's runtime
    /// [`Config`], applying every default in one place. This is the single
    /// boundary between the all-optional config-file shape and the
    /// fully-populated config the engine runs on — callers downstream never see
    /// an `Option` or a default fallback.
    ///
    /// `providers`/`drivers` are intentionally *not* part of [`Config`] (they are
    /// applied to the engine registry separately, post-construction), so they
    /// stay on the [`ConfigYaml`].
    fn resolve(&self, root: &Path) -> anyhow::Result<Config> {
        let mem_cache_opts = |c: &MemCacheConfig| MemCacheOptions {
            per_entry_bytes: c.per_entry_bytes,
            capacity_bytes: c.capacity_bytes,
        };

        let defaults = Config::default();
        Ok(Config {
            root: root.to_path_buf(),
            home_dir: self
                .home_dir
                .as_ref()
                .map(|p| root.join(p))
                .unwrap_or_else(|| root.join(".heph3")),
            fs_skip: self.fs.as_ref().map(|f| f.skip.clone()).unwrap_or_default(),
            parallelism: None,
            mem_cache: self
                .mem_cache
                .as_ref()
                .map(mem_cache_opts)
                .unwrap_or(defaults.mem_cache),
            tmp_cache: self
                .tmp_cache
                .as_ref()
                .map(mem_cache_opts)
                .unwrap_or(defaults.tmp_cache),
            fuse: self.fuse.unwrap_or(defaults.fuse),
            lock_backend: self
                .lock
                .and_then(|l| l.backend)
                .map(|b| match b {
                    LockBackendConfig::Fs => LockBackend::Fs,
                    LockBackendConfig::Mem => LockBackend::Mem,
                })
                .unwrap_or(defaults.lock_backend),
            spill_threshold_bytes: self
                .cache
                .and_then(|c| c.spill_threshold_bytes)
                .unwrap_or(defaults.spill_threshold_bytes),
            telemetry_enabled: self.telemetry_enabled(),
            remote_caches: self
                .resolved_caches()?
                .into_iter()
                .map(|(name, c)| RemoteCacheDef {
                    name,
                    uri: c.uri,
                    read: c.read,
                    write: c.write,
                    concurrency: c.concurrency,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::result_lock::LockBackend;

    #[test]
    fn resolve_applies_defaults_for_empty_yaml() {
        // An empty config resolves to the engine defaults, with home_dir
        // root-joined to `.heph3`.
        let yaml = ConfigYaml::default();
        let root = Path::new("/repo");
        let cfg = yaml.resolve(root).expect("resolve");

        let defaults = Config::default();
        assert_eq!(cfg.root, root);
        assert_eq!(cfg.home_dir, root.join(".heph3"));
        assert_eq!(cfg.mem_cache, defaults.mem_cache);
        assert_eq!(cfg.tmp_cache, defaults.tmp_cache);
        assert_eq!(cfg.lock_backend, defaults.lock_backend);
        assert_eq!(cfg.spill_threshold_bytes, defaults.spill_threshold_bytes);
        assert!(cfg.telemetry_enabled);
        assert!(cfg.remote_caches.is_empty());
    }

    #[test]
    fn resolve_overrides_present_fields() {
        let yaml: ConfigYaml = serde_yaml::from_str(
            "homeDir: .custom\nlock:\n  backend: mem\ntelemetry:\n  enabled: false\ncaches:\n  r:\n    uri: s3://b/p\n    write: false\n",
        )
        .expect("parse");
        let cfg = yaml.resolve(Path::new("/repo")).expect("resolve");

        assert_eq!(cfg.home_dir, Path::new("/repo/.custom"));
        assert_eq!(cfg.lock_backend, LockBackend::Mem);
        assert!(!cfg.telemetry_enabled);
        assert_eq!(cfg.remote_caches.len(), 1);
        let r = &cfg.remote_caches[0];
        assert_eq!(r.uri, "s3://b/p");
        assert!(r.read);
        assert!(!r.write);
    }

    #[test]
    fn resolve_errors_on_cache_missing_uri() {
        let yaml: ConfigYaml =
            serde_yaml::from_str("caches:\n  r:\n    write: false\n").expect("parse");
        let err = yaml.resolve(Path::new("/repo")).expect_err("must error");
        assert!(format!("{err:#}").contains("uri"), "{err}");
    }
}
