//! The engine's **resolved** runtime configuration.
//!
//! This is the counterpart to [`ConfigYaml`](crate::engine::config_yaml::ConfigYaml):
//! where the YAML layer is all-optional and built up by layering profiles, this
//! layer has *no* optional fields — every default is applied exactly once, in
//! [`ConfigYaml::resolve`]. Everything downstream of [`resolve`](ConfigYaml::resolve)
//! sees a fully-populated [`Config`] and never has to reason about defaults.

use std::path::{Path, PathBuf};

use crate::engine::RemoteCacheDef;
use crate::engine::config_yaml::{
    ConfigYaml, FuseConfig, LockBackendConfig, MemCacheConfig, ScratchConfig,
};
use crate::engine::result_lock::LockBackend;

/// Expand a configured scope, resolving `${git:branch}` against `root`.
///
/// Done here so nothing downstream knows about git: a scope reaches the store as
/// a plain string, and a workspace that is not a git checkout simply gets an
/// empty scope (one shared lineage) rather than an error — a cache policy must
/// never be the reason a build cannot start.
fn expand_scope(raw: &str, root: &Path) -> String {
    const GIT_BRANCH: &str = "${git:branch}";
    if !raw.contains(GIT_BRANCH) {
        return raw.to_string();
    }
    let branch = git_branch(root).unwrap_or_default();
    raw.replace(GIT_BRANCH, &branch)
}

/// The current branch name, or `None` outside a git checkout / on a detached
/// HEAD. Reads `.git/HEAD` directly rather than shelling out to `git`: this runs
/// on every engine construction, and a subprocess for one line of a file that is
/// always there is not worth it — nor is depending on `git` being installed.
fn git_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    // `ref: refs/heads/<branch>` on a branch; a bare sha when detached, which is
    // not a lineage anyone means to share, so it reads as no scope.
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Make a scope safe to use as one path component.
///
/// Branch names routinely contain `/` (`feature/x`), and a raw one would silently
/// nest the store an extra level and make two branches collide the moment one is a
/// prefix path of another. Same sanitizer shape as the env-var one: keep what is
/// unambiguous, fold the rest to `_`.
pub fn sanitize_scope(scope: &str) -> String {
    if scope.is_empty() {
        return "_".to_string();
    }
    scope
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `c` is `None` when the config file has no `scratch:` section at all — the
/// common case, and still worth resolving here rather than through a separate
/// `unwrap_or(defaults)` branch that would be one place for the two paths to
/// drift apart.
fn resolve_scratch(c: Option<&ScratchConfig>, root: &Path) -> ScratchOptions {
    let defaults = ScratchOptions::default();
    ScratchOptions {
        scope: c
            .and_then(|c| c.scope.as_deref())
            .map(|s| expand_scope(s, root))
            .unwrap_or(defaults.scope),
        restore_scopes: c.map(|c| c.restore_scopes.clone()).unwrap_or_default(),
        seed_on_fork: c
            .and_then(|c| c.seed_on_fork)
            .unwrap_or(defaults.seed_on_fork),
    }
}

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
    /// Override for who is running this build (`cache.subject_scoped`).
    ///
    /// Detected from the environment when unset. Present because the detection
    /// reads process-global state, which a test cannot set without racing every
    /// other test in its binary — and a cache-key input deserves a test that is
    /// not a coin flip.
    pub run_subject: Option<String>,
    /// Where `heph auth login` signs in, paired with the per-user heph home.
    ///
    /// Resolved once here rather than looked up at mint time: a provider that
    /// went reading `.hephconfig` and `$HOME` itself would behave differently
    /// depending on where the build was invoked, which is exactly what
    /// [`hsecrets::MintCtx`] exists to prevent. `None` when the workspace
    /// configures no `auth:` block, or when `$HOME` is unset — neither is an
    /// error unless a descriptor actually needs a session, so the diagnostic
    /// belongs there rather than here.
    pub auth: Option<hsecrets::provider::AuthContext>,
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
    /// Which scratch lineage this run reads and writes. See [`ScratchOptions`].
    pub scratch: ScratchOptions,
}

/// Resolved scratch-lineage policy. The config's `${git:branch}` is already
/// expanded here, so nothing downstream knows about git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchOptions {
    /// Lineage this run writes to. Empty means one shared lineage.
    pub scope: String,
    /// Lineages to fall back to on read, in order. Never written to.
    pub restore_scopes: Vec<String>,
    /// Seed a new scope from its fallback rather than starting cold.
    pub seed_on_fork: bool,
}

impl Default for ScratchOptions {
    fn default() -> Self {
        Self {
            scope: String::new(),
            restore_scopes: Vec::new(),
            // On, because branch scoping without it makes every switch cold,
            // which is worse than not scoping at all.
            seed_on_fork: true,
        }
    }
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
            run_subject: None,
            mem_cache: MemCacheOptions::default(),
            tmp_cache: MemCacheOptions::default_tmp(),
            fuse: FuseConfig::default(),
            lock_backend: LockBackend::default(),
            spill_threshold_bytes: DEFAULT_SPILL_THRESHOLD_BYTES,
            telemetry_enabled: true,
            remote_caches: Vec::new(),
            scratch: ScratchOptions::default(),
            auth: None,
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
            run_subject: None,
            auth: self.auth.clone().and_then(|config| {
                // A machine with no `$HOME` cannot hold a session; that is not a
                // reason to fail a build that never asks for one.
                hsecrets::Session::home()
                    .ok()
                    .map(|home| hsecrets::provider::AuthContext { config, home })
            }),
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
            scratch: resolve_scratch(self.scratch.as_ref(), root),
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
                    endpoint: c.endpoint,
                    region: c.region,
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
    fn a_scope_without_the_placeholder_is_passed_through() {
        assert_eq!(expand_scope("main", Path::new("/nope")), "main");
        assert_eq!(expand_scope("", Path::new("/nope")), "");
    }

    /// A workspace that is not a git checkout must still build. A cache policy is
    /// never a reason a run cannot start, so an unresolvable branch reads as "no
    /// scope" — one shared lineage — rather than an error.
    #[test]
    fn git_branch_outside_a_checkout_expands_to_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(expand_scope("${git:branch}", tmp.path()), "");
    }

    #[test]
    fn git_branch_reads_the_current_branch_from_head() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".git")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".git").join("HEAD"),
            "ref: refs/heads/feature/x\n",
        )
        .expect("write");
        assert_eq!(expand_scope("${git:branch}", tmp.path()), "feature/x");
        // Composable, so a CI config can namespace a shared bucket.
        assert_eq!(expand_scope("ci-${git:branch}", tmp.path()), "ci-feature/x");
    }

    /// A detached HEAD is a sha, not a lineage anyone means to share.
    #[test]
    fn a_detached_head_expands_to_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".git")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".git").join("HEAD"),
            "9f1027fc595e9705ae0ec764cd6264b07e5271c0\n",
        )
        .expect("write");
        assert_eq!(expand_scope("${git:branch}", tmp.path()), "");
    }

    #[test]
    fn sanitize_scope_keeps_one_path_component() {
        assert_eq!(sanitize_scope("main"), "main");
        assert_eq!(sanitize_scope("feature/x"), "feature_x");
        assert_eq!(sanitize_scope("v1.2-rc_3"), "v1.2-rc_3");
        // The empty scope is the default and still needs a name of its own.
        assert_eq!(sanitize_scope(""), "_");
        // No traversal can come out of a branch name.
        assert!(!sanitize_scope("../../etc").contains('/'));
        assert!(!sanitize_scope("..").contains('.') || sanitize_scope("..") == "..");
    }

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
    fn resolve_carries_cache_endpoint_and_region() {
        let yaml: ConfigYaml = serde_yaml::from_str(concat!(
            "caches:\n",
            "  r:\n",
            "    uri: s3://b/p\n",
            "    endpoint: https://accountid.r2.cloudflarestorage.com\n",
            "    region: auto\n",
        ))
        .expect("parse");
        let cfg = yaml.resolve(Path::new("/repo")).expect("resolve");
        let r = &cfg.remote_caches[0];
        assert_eq!(
            r.endpoint.as_deref(),
            Some("https://accountid.r2.cloudflarestorage.com")
        );
        assert_eq!(r.region.as_deref(), Some("auto"));
    }

    #[test]
    fn resolve_errors_on_cache_missing_uri() {
        let yaml: ConfigYaml =
            serde_yaml::from_str("caches:\n  r:\n    write: false\n").expect("parse");
        let err = yaml.resolve(Path::new("/repo")).expect_err("must error");
        assert!(format!("{err:#}").contains("uri"), "{err}");
    }

    /// A config file with no `scratch:` section still resolves a usable lineage
    /// policy. This used to take a separate `unwrap_or(defaults)` branch that
    /// skipped `resolve_scratch` entirely — one place for the two paths to
    /// drift apart.
    #[test]
    fn an_absent_scratch_section_still_resolves_a_lineage_policy() {
        let bare = ConfigYaml::default()
            .resolve(Path::new("/repo"))
            .expect("resolve");
        assert_eq!(bare.scratch, ScratchOptions::default());

        let configured = ConfigYaml {
            scratch: Some(ScratchConfig {
                scope: Some("main".into()),
                restore_scopes: vec!["master".into()],
                seed_on_fork: Some(false),
            }),
            ..Default::default()
        }
        .resolve(Path::new("/repo"))
        .expect("resolve");
        assert_eq!(configured.scratch.scope, "main");
        assert_eq!(
            configured.scratch.restore_scopes,
            vec!["master".to_string()]
        );
        assert!(!configured.scratch.seed_on_fork);
    }
}
