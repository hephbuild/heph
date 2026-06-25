use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::options::Options;

/// Default cap on in-flight requests to one remote cache (object_store
/// `LimitStore`). Lives here because the YAML layer applies it as the default for
/// an omitted `concurrency`; the engine re-exports it for its own callers.
pub const DEFAULT_CACHE_CONCURRENCY: usize = 10;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConfigYaml {
    /// Pinned heph version (or, in future, a version constraint) this workspace
    /// expects. When set and it differs from the running binary, the self-upgrade
    /// check downloads the pinned release and re-execs into it. Omit to disable
    /// self-upgrade for the workspace.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub home_dir: Option<PathBuf>,
    /// Every provider/driver — built-in or external — is declared as a single
    /// `plugin`. A `builtin:` entry selects a compiled-in plugin by name; a
    /// `path:`/`url:` entry points at a `*-plugin.json` manifest for a loadable
    /// cdylib. Each carries an `options:` map passed to plugin construction.
    #[serde(default)]
    pub plugins: Vec<PluginSpec>,
    #[serde(default)]
    pub mem_cache: Option<MemCacheConfig>,
    #[serde(default)]
    pub tmp_cache: Option<MemCacheConfig>,
    #[serde(default)]
    pub fuse: Option<FuseConfig>,
    #[serde(default)]
    pub lock: Option<LockConfig>,
    #[serde(default)]
    pub fs: Option<FsConfig>,
    #[serde(default)]
    pub cache: Option<CacheConfig>,
    /// Named remote (shared) caches. Each entry is keyed by a name and carries a
    /// `uri` plus `read`/`write` permissions. Parsed as patches so a profile can
    /// override individual fields; resolve with [`ConfigYaml::resolved_caches`].
    #[serde(default)]
    pub caches: BTreeMap<String, RemoteCacheConfigPatch>,
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
}

/// One resolved named remote cache: `caches: { name: { uri, read, write } }`.
///
/// `uri` selects the backend by scheme — `s3://bucket/prefix`,
/// `gs://bucket/prefix` (credentials come from the environment, e.g.
/// `AWS_ACCESS_KEY_ID` / `GOOGLE_SERVICE_ACCOUNT`), plus `memory://` and
/// `file://` for tests/local use. `read`/`write` gate whether the cache is
/// consulted on lookups and pushed to on writes; both default to `true`.
///
/// Produced from one or more layered [`RemoteCacheConfigPatch`]es — see
/// [`ConfigYaml::resolved_caches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCacheConfig {
    pub uri: String,
    pub read: bool,
    pub write: bool,
    /// Max in-flight requests to this cache (object_store `LimitStore`). Caps how
    /// many connections a wide build fan-out opens at once. Defaults to 10.
    pub concurrency: usize,
}

/// Parse/overlay shape for one named cache. Every field is optional so a profile
/// can patch a single setting (e.g. flip `write`) without restating `uri`.
/// Layered patches are deep-merged by [`merge`](Self::merge), then finalized into
/// a [`RemoteCacheConfig`] by [`resolve`](Self::resolve).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteCacheConfigPatch {
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub read: Option<bool>,
    #[serde(default)]
    pub write: Option<bool>,
    #[serde(default)]
    pub concurrency: Option<usize>,
}

impl RemoteCacheConfigPatch {
    /// Apply `other` on top of self — each field present in `other` wins, the
    /// rest are left untouched. This is what lets a profile flip `read`/`write`
    /// while inheriting the base `uri`.
    fn merge(&mut self, other: RemoteCacheConfigPatch) {
        if other.uri.is_some() {
            self.uri = other.uri;
        }
        if other.read.is_some() {
            self.read = other.read;
        }
        if other.write.is_some() {
            self.write = other.write;
        }
        if other.concurrency.is_some() {
            self.concurrency = other.concurrency;
        }
    }

    /// Finalize into a [`RemoteCacheConfig`], applying defaults for omitted
    /// fields. Errors if `uri` was never set across the base + any profiles.
    fn resolve(&self, name: &str) -> anyhow::Result<RemoteCacheConfig> {
        let uri = self
            .uri
            .clone()
            .with_context(|| format!("cache `{name}` is missing required `uri`"))?;
        Ok(RemoteCacheConfig {
            uri,
            read: self.read.unwrap_or(true),
            write: self.write.unwrap_or(true),
            concurrency: self.concurrency.unwrap_or(DEFAULT_CACHE_CONCURRENCY),
        })
    }
}

/// Durable local-cache tuning. `cache: { spillThresholdBytes: N }`.
#[derive(Debug, Deserialize, Default, Clone, Copy)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CacheConfig {
    /// Blobs strictly larger than this spill to plain files under
    /// `<home>/cache/blobs/` instead of being stored inline in the sqlite DB.
    /// Manifests always stay in sqlite. Omit to use the engine default.
    #[serde(default)]
    pub spill_threshold_bytes: Option<u64>,
}

impl ConfigYaml {
    /// Whether anonymous usage telemetry is enabled. Opt-out: enabled unless the
    /// config explicitly sets `telemetry.enabled: false`.
    pub fn telemetry_enabled(&self) -> bool {
        self.telemetry.map(|t| t.enabled).unwrap_or(true)
    }

    /// Finalize the layered cache patches into resolved configs, applying field
    /// defaults. Errors if any cache never had its required `uri` set.
    pub fn resolved_caches(&self) -> anyhow::Result<BTreeMap<String, RemoteCacheConfig>> {
        self.caches
            .iter()
            .map(|(name, patch)| Ok((name.clone(), patch.resolve(name)?)))
            .collect()
    }

    /// Apply `other` on top of `self`, mutating in place. Used to layer profile
    /// configs over the base `.hephconfig` (see [`load_from_root`]).
    ///
    /// Optional/scalar fields override only when present in `other` — a profile
    /// that omits a field leaves the base value untouched. `caches` deep-merge by
    /// key (each cache's fields are patched individually, so a profile can flip
    /// `read`/`write` while inheriting the base `uri`), and `plugins` merge by
    /// identity (matching entry replaced, new ones appended in order).
    pub fn merge(&mut self, other: ConfigYaml) {
        if other.version.is_some() {
            self.version = other.version;
        }
        if other.home_dir.is_some() {
            self.home_dir = other.home_dir;
        }
        if other.mem_cache.is_some() {
            self.mem_cache = other.mem_cache;
        }
        if other.tmp_cache.is_some() {
            self.tmp_cache = other.tmp_cache;
        }
        if other.fuse.is_some() {
            self.fuse = other.fuse;
        }
        if other.lock.is_some() {
            self.lock = other.lock;
        }
        if other.fs.is_some() {
            self.fs = other.fs;
        }
        if other.cache.is_some() {
            self.cache = other.cache;
        }
        if other.telemetry.is_some() {
            self.telemetry = other.telemetry;
        }

        // Caches deep-merge by key: a profile patches individual fields (e.g.
        // flips read/write) while inheriting the rest of the base entry.
        for (name, patch) in other.caches {
            self.caches.entry(name).or_default().merge(patch);
        }

        merge_plugins(&mut self.plugins, other.plugins);
    }
}

/// Merge `inc` plugin entries into `base` by `identifier`: an entry whose
/// identifier already exists replaces it in place; a new one is appended,
/// preserving order. Lets a profile override a plugin's options.
fn merge_plugins(base: &mut Vec<PluginSpec>, inc: Vec<PluginSpec>) {
    for entry in inc {
        match base.iter_mut().find(|e| e.identifier == entry.identifier) {
            Some(existing) => *existing = entry,
            None => base.push(entry),
        }
    }
}

/// Anonymous usage telemetry. `telemetry: { enabled: false }` opts out; omitting
/// the block (or the field) leaves telemetry on. No data here identifies a user
/// — only os/arch/version and aggregate run counters are ever reported.
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TelemetryConfig {
    #[serde(default = "default_telemetry_enabled")]
    pub enabled: bool,
}

fn default_telemetry_enabled() -> bool {
    true
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: default_telemetry_enabled(),
        }
    }
}

/// Filesystem-walk config shared across plugins. `fs: { skip: [dir, ...] }`.
/// Each `skip` entry is a directory path relative to the repo root (no globs);
/// the engine resolves them to absolute paths and hands them to every plugin
/// that walks the tree (the built-in `fs` plugin, the buildfile/go providers),
/// so every walk prunes the same directories.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FsConfig {
    #[serde(default)]
    pub skip: Vec<String>,
}

/// Byte limits for one in-memory cache store. Shared shape for `mem_cache`
/// (the local-cache mem tier) and `tmp_cache` (the tmp/uncacheable store).
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MemCacheConfig {
    pub per_entry_bytes: usize,
    /// Total byte budget. For `mem_cache`, `0` disables the layer entirely; for
    /// `tmp_cache`, entries that would exceed it spill to the durable cache.
    pub capacity_bytes: u64,
}

/// The resolved FUSE-overlay decision the managed-driver bridge acts on,
/// independent of the YAML shape. Owned here (central config) since
/// [`FuseConfig::mode`] resolves to it; `hdriver_bridge::fuseconfig` re-exports
/// it for the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseMode {
    /// Forced on. Probe must succeed; mount errors propagate.
    On,
    /// Forced off.
    Off,
    /// Engine decides per-target by walking inputs.
    Auto,
}

/// Sandbox FUSE-overlay mode. `fuse: { enabled: true | false | auto }`
/// selects mode explicitly. Omit `enabled` (or the entire `fuse:` block) to
/// default to off; FUSE is opt-in.
///
/// `backend` (macOS only) selects the userspace FUSE backend: `kext` (the
/// classic kernel-extension path — fastest, but the macFUSE system extension
/// must be approved) or `fskit` (Apple's FSKit, macOS 15.4+ — unprivileged,
/// no kext, mounts under `/Volumes`). Omit to let the engine pick the backend
/// that is actually usable on the host. Ignored on Linux.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FuseConfig {
    #[serde(default)]
    pub enabled: Option<FuseEnabled>,
    #[serde(default)]
    pub backend: Option<FuseBackend>,
}

/// macOS FUSE backend selector. See [`FuseConfig`].
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FuseBackend {
    /// Classic kernel-extension backend (macFUSE kext).
    Kext,
    /// Apple FSKit backend — unprivileged, macOS 15.4+.
    Fskit,
}

/// Tri-state config value. Parses YAML `true`, `false`, or `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseEnabled {
    On,
    Off,
    Auto,
}

impl<'de> serde::Deserialize<'de> for FuseEnabled {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{Error as DeError, Visitor};
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FuseEnabled;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "true, false, or \"auto\"")
            }
            fn visit_bool<E: DeError>(self, v: bool) -> Result<FuseEnabled, E> {
                Ok(if v { FuseEnabled::On } else { FuseEnabled::Off })
            }
            fn visit_str<E: DeError>(self, v: &str) -> Result<FuseEnabled, E> {
                match v {
                    "auto" => Ok(FuseEnabled::Auto),
                    "true" | "on" => Ok(FuseEnabled::On),
                    "false" | "off" => Ok(FuseEnabled::Off),
                    other => Err(E::custom(format!(
                        "expected true/false/auto, got {other:?}"
                    ))),
                }
            }
        }
        d.deserialize_any(V)
    }
}

impl FuseConfig {
    /// Resolve the YAML config to the bridge's decision enum.
    pub fn mode(&self) -> FuseMode {
        match self.enabled {
            Some(FuseEnabled::On) => FuseMode::On,
            Some(FuseEnabled::Auto) => FuseMode::Auto,
            Some(FuseEnabled::Off) | None => FuseMode::Off,
        }
    }

    /// Convenience: FUSE off (explicit `enabled: false` or omitted).
    pub fn is_off(&self) -> bool {
        matches!(self.mode(), FuseMode::Off)
    }

    /// Convenience: is FUSE forced on by config?
    pub fn is_on(&self) -> bool {
        matches!(self.mode(), FuseMode::On)
    }

    /// Build a config forced on (tests / programmatic callers).
    pub fn on() -> Self {
        Self {
            enabled: Some(FuseEnabled::On),
            backend: None,
        }
    }

    /// Build a config in `auto` mode (tests / programmatic callers).
    pub fn auto() -> Self {
        Self {
            enabled: Some(FuseEnabled::Auto),
            backend: None,
        }
    }

    /// Force a specific macOS backend, keeping the current mode.
    pub fn with_backend(mut self, backend: FuseBackend) -> Self {
        self.backend = Some(backend);
        self
    }
}

/// Execute-phase lock backend. `lock: { backend: fs | mem }`. Omit the block (or
/// `backend`) to default to the filesystem backend.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LockConfig {
    #[serde(default)]
    pub backend: Option<LockBackendConfig>,
}

/// YAML spelling of the lock backend. Maps to `engine::LockBackend`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LockBackendConfig {
    Fs,
    Mem,
}

/// One `plugins:` entry. In YAML the plugin source is given inline as one of
/// `builtin:` / `path:` / `url:` at the top of the entry, plus an optional
/// `options:` config map passed to construction:
///
/// ```yaml
/// plugins:
///   - builtin: buildfile
///     options: { patterns: [BUILD] }
///   - path: .heph3/heph-go-plugin.json
///   - url: https://…/heph-go-plugin.json
///     checksum: sha256:9f86d0…
/// ```
///
/// The source collapses to an internal [`PluginIdentifier`]; `identifier` is not
/// a YAML key.
///
/// `checksum` pins the *manifest* fetched from a `url:` source (format
/// `sha256:<hex>`). It is only meaningful for remote manifests, so it is rejected
/// alongside `builtin:`/`path:`. Per-host artifact (cdylib) integrity is pinned
/// separately by the manifest itself (`artifacts[].checksum`), giving a full
/// trust chain: config pins the manifest, the manifest pins each artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginSpec {
    pub identifier: PluginIdentifier,
    pub options: Options,
    /// Expected checksum of the downloaded manifest (`sha256:<hex>`), for `url:`
    /// sources only. `None` skips manifest verification.
    pub checksum: Option<String>,
}

// Hand-rolled so the source kind (builtin/path/url) sits at the top of the entry
// alongside `options`, exactly-one is enforced at parse time, and unknown keys
// are rejected.
impl<'de> Deserialize<'de> for PluginSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let mut map: BTreeMap<String, serde_yaml::Value> = BTreeMap::deserialize(d)?;

        let options = match map.remove("options") {
            Some(v) => serde_yaml::from_value(v)
                .map_err(|e| D::Error::custom(format!("plugin `options`: {e}")))?,
            None => Options::default(),
        };

        let checksum = match map.remove("checksum") {
            Some(v) => Some(serde_yaml::from_value::<String>(v).map_err(|e| {
                D::Error::custom(format!("plugin `checksum` must be a string: {e}"))
            })?),
            None => None,
        };

        let mut source: Option<PluginIdentifier> = None;
        for (key, value) in map {
            let id = match key.as_str() {
                "builtin" | "path" | "url" => {
                    let s: String = serde_yaml::from_value(value).map_err(|e| {
                        D::Error::custom(format!("plugin `{key}` must be a string: {e}"))
                    })?;
                    match key.as_str() {
                        "builtin" => PluginIdentifier::Builtin(s),
                        "path" => PluginIdentifier::Path(s),
                        _ => PluginIdentifier::Url(s),
                    }
                }
                other => {
                    return Err(D::Error::custom(format!(
                        "unknown plugin field `{other}` (expected builtin/path/url/options/checksum)"
                    )));
                }
            };
            if source.replace(id).is_some() {
                return Err(D::Error::custom(
                    "plugin entry must set exactly one of builtin/path/url",
                ));
            }
        }

        let identifier = source
            .ok_or_else(|| D::Error::custom("plugin entry must set one of builtin/path/url"))?;
        // `checksum` pins a downloaded manifest, so it only applies to `url:`.
        if checksum.is_some() && !matches!(identifier, PluginIdentifier::Url(_)) {
            return Err(D::Error::custom(
                "plugin `checksum` is only valid with a `url:` source",
            ));
        }
        Ok(PluginSpec {
            identifier,
            options,
            checksum,
        })
    }
}

/// What plugin to load — internal representation of an entry's inline
/// `builtin:`/`path:`/`url:` source (see [`PluginSpec`]).
/// - `builtin`: a compiled-in plugin by name (e.g. `buildfile`, `exec`).
/// - `path`: a local `*-plugin.json` manifest (relative to the workspace root).
/// - `url`: a remote `*-plugin.json` manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginIdentifier {
    Builtin(String),
    Path(String),
    Url(String),
}

/// Canonical filename of the per-workspace config, located at the repo root.
/// New workspaces should use this name.
pub const CONFIG_FILE_NAME: &str = ".hephconfig";

/// Migration filename written when porting a legacy repo into this version of
/// heph. While present it takes precedence over [`CONFIG_FILE_NAME`], so a repo
/// mid-migration runs against the ported config until the legacy file is removed.
pub const CONFIG_FILE_NAME_LEGACY: &str = ".hephconfig2";

/// Candidate config filenames in priority order. The single source of truth for
/// these names — [`load_from_root`] and the root discovery in [`get_root`] both
/// build on it. The migration `.hephconfig2` wins when both exist; the canonical
/// `.hephconfig` is read only when no `.hephconfig2` is present. The chosen name
/// also drives the profile filenames (see [`load_with_profiles`]).
pub const CONFIG_FILE_NAMES: &[&str] = &[CONFIG_FILE_NAME_LEGACY, CONFIG_FILE_NAME];

/// Pick the config filename present at `root`, preferring the migration
/// [`CONFIG_FILE_NAME_LEGACY`] over the canonical [`CONFIG_FILE_NAME`]. Falls back
/// to [`CONFIG_FILE_NAME`] when neither exists so callers still get a sensible
/// path for error messages.
pub fn config_file_name_at(root: &Path) -> &'static str {
    CONFIG_FILE_NAMES
        .iter()
        .copied()
        .find(|name| root.join(name).exists())
        .unwrap_or(CONFIG_FILE_NAME)
}

/// Env var naming the comma-separated list of config profiles to layer on top of
/// the base config, in order. See [`load_from_root`].
pub const PROFILES_ENV: &str = "HEPH_PROFILES";

/// Profiles requested via `HEPH_PROFILES`, in order. Empty/whitespace entries are
/// dropped, so `HEPH_PROFILES=a,,b ` yields `["a", "b"]`.
fn profiles_from_env() -> Vec<String> {
    std::env::var(PROFILES_ENV)
        .ok()
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Load the workspace config from `<root>/.hephconfig2` when present (the
/// migration file), otherwise the canonical `<root>/.hephconfig`, then layer each
/// profile named in `HEPH_PROFILES` on top.
///
/// Profiles apply in the order listed: `HEPH_PROFILES=a,b` loads the base config,
/// then applies `a<suffix>`, then `b<suffix>`, each merged over the accumulated
/// config via [`ConfigYaml::merge`] (so e.g. a cache entry is overridden by its
/// key). The profile suffix tracks the base file actually chosen, so a legacy
/// `.hephconfig2` workspace gets `a.hephconfig2` profiles. A named profile file
/// that is missing or invalid is a hard error.
pub fn load_from_root(root: &Path) -> anyhow::Result<ConfigYaml> {
    load_with_profiles(root, &profiles_from_env())
}

/// Core of [`load_from_root`], with the profile list passed in so it can be
/// exercised without touching the process environment.
fn load_with_profiles(root: &Path, profiles: &[impl AsRef<str>]) -> anyhow::Result<ConfigYaml> {
    let base_name = config_file_name_at(root);
    let mut cfg = load(&root.join(base_name))?;
    for profile in profiles {
        let profile = profile.as_ref();
        // base_name carries the leading dot, so this is `<profile>.hephconfig`
        // (or `<profile>.hephconfig2` for a legacy workspace).
        let file_name = format!("{profile}{base_name}");
        tracing::debug!(profile, file = %file_name, "applying config profile");
        let overlay = load(&root.join(&file_name))
            .with_context(|| format!("loading config profile {file_name}"))?;
        cfg.merge(overlay);
    }
    Ok(cfg)
}

pub fn load(path: &Path) -> anyhow::Result<ConfigYaml> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading config file {}", path.display()))?;
    if bytes.is_empty() {
        return Ok(ConfigYaml::default());
    }
    let cfg: ConfigYaml = serde_yaml::from_slice(&bytes)
        .with_context(|| format!("parsing YAML config {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{decode_opt, deny_unknown};

    #[test]
    fn parses_full_example() {
        let yaml = r#"
homeDir: .heph2
memCache:
  perEntryBytes: 16384
  capacityBytes: 67108864
tmpCache:
  perEntryBytes: 1048576
  capacityBytes: 134217728
plugins:
  - builtin: buildfile
    options:
      patterns:
        - BUILD2
        - "*.BUILD2"
  - builtin: exec
    options:
      path:
        - /usr/sbin
        - /usr/bin
  - path: .heph3/heph-go-plugin.json
    options:
      gotool: //tools/heph:go
"#;
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.home_dir.as_deref(), Some(Path::new(".heph2")));
        let mc = cfg.mem_cache.expect("mem_cache present");
        assert_eq!(mc.per_entry_bytes, 16384);
        assert_eq!(mc.capacity_bytes, 67108864);
        let tc = cfg.tmp_cache.expect("tmp_cache present");
        assert_eq!(tc.per_entry_bytes, 1048576);
        assert_eq!(tc.capacity_bytes, 134217728);
        assert_eq!(cfg.plugins.len(), 3);
        assert_eq!(
            cfg.plugins[0].identifier,
            PluginIdentifier::Builtin("buildfile".into())
        );
        let patterns: Vec<String> = decode_opt(&cfg.plugins[0].options, "buildfile", "patterns")
            .expect("decode")
            .expect("present");
        assert_eq!(patterns, vec!["BUILD2".to_string(), "*.BUILD2".to_string()]);
        assert_eq!(
            cfg.plugins[2].identifier,
            PluginIdentifier::Path(".heph3/heph-go-plugin.json".into())
        );
    }

    #[test]
    fn parses_version_field() {
        let cfg: ConfigYaml = serde_yaml::from_str("version: v1.2.3\n").expect("parse");
        assert_eq!(cfg.version.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn version_omitted_is_none() {
        let cfg: ConfigYaml = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.version.is_none());
    }

    #[test]
    fn merge_overrides_version_only_when_present() {
        let mut base: ConfigYaml = serde_yaml::from_str("version: v1.0.0\n").expect("parse base");
        // Overlay without `version` leaves the base pin untouched.
        let overlay: ConfigYaml = serde_yaml::from_str("homeDir: .prof\n").expect("parse overlay");
        base.merge(overlay);
        assert_eq!(base.version.as_deref(), Some("v1.0.0"));

        // Overlay with `version` wins.
        let overlay2: ConfigYaml = serde_yaml::from_str("version: v2.0.0\n").expect("parse o2");
        base.merge(overlay2);
        assert_eq!(base.version.as_deref(), Some("v2.0.0"));
    }

    #[test]
    fn parses_cache_spill_threshold() {
        let yaml = "cache:\n  spillThresholdBytes: 104857600\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let c = cfg.cache.expect("cache present");
        assert_eq!(c.spill_threshold_bytes, Some(104857600));
    }

    #[test]
    fn cache_config_omitted_is_none() {
        let cfg: ConfigYaml = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.cache.is_none());
    }

    #[test]
    fn rejects_unknown_cache_field() {
        let err = serde_yaml::from_str::<ConfigYaml>("cache:\n  bogus: 1\n").expect_err("reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn parses_named_caches() {
        let yaml = r#"
caches:
  remote:
    uri: s3://my-bucket/heph
    read: true
    write: false
  local:
    uri: file:///tmp/heph-cache
"#;
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.caches.len(), 2);
        let caches = cfg.resolved_caches().expect("resolve");
        let remote = caches.get("remote").expect("remote present");
        assert_eq!(remote.uri, "s3://my-bucket/heph");
        assert!(remote.read);
        assert!(!remote.write);
        // read/write/concurrency default when omitted.
        let local = caches.get("local").expect("local present");
        assert!(local.read);
        assert!(local.write);
        assert_eq!(local.concurrency, 10);
    }

    #[test]
    fn cache_concurrency_is_configurable() {
        let yaml = "caches:\n  c:\n    uri: s3://b/p\n    concurrency: 32\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let caches = cfg.resolved_caches().expect("resolve");
        assert_eq!(caches.get("c").expect("present").concurrency, 32);
    }

    #[test]
    fn resolved_caches_errors_on_missing_uri() {
        // A cache that only ever sets read/write (no base uri) cannot resolve.
        let cfg: ConfigYaml =
            serde_yaml::from_str("caches:\n  c:\n    read: false\n").expect("parse");
        let err = cfg.resolved_caches().expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("`c`") && msg.contains("uri"), "{msg}");
    }

    #[test]
    fn caches_omitted_is_empty() {
        let cfg: ConfigYaml = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.caches.is_empty());
    }

    #[test]
    fn rejects_unknown_cache_entry_field() {
        let yaml = "caches:\n  r:\n    uri: memory:///r\n    bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn parses_fs_skip() {
        // `skip` mixes literal dirs and glob patterns.
        let yaml = "fs:\n  skip: [vendor, \"**/node_modules/**\"]\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let fs = cfg.fs.expect("fs config present");
        assert_eq!(
            fs.skip,
            vec!["vendor".to_string(), "**/node_modules/**".to_string()]
        );
    }

    #[test]
    fn fs_config_defaults_absent() {
        let cfg: ConfigYaml = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.fs.is_none());
    }

    #[test]
    fn rejects_unknown_fs_field() {
        let err = serde_yaml::from_str::<ConfigYaml>("fs:\n  bogus: 1\n").expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn empty_file_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".hephconfig2");
        std::fs::write(&path, b"").expect("write");
        let cfg = load(&path).expect("load");
        assert!(cfg.home_dir.is_none());
        assert!(cfg.plugins.is_empty());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let yaml = "bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn deny_unknown_errors_on_extra_keys() {
        let mut opts = Options::new();
        opts.insert("foo".to_string(), serde_yaml::Value::Bool(true));
        let err = deny_unknown("test", &opts, &["bar"]).expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("unknown option `foo`"), "{msg}");
        assert!(msg.contains("allowed: bar"), "{msg}");
    }

    #[test]
    fn deny_unknown_passes_on_allowed_keys() {
        let mut opts = Options::new();
        opts.insert("bar".to_string(), serde_yaml::Value::Bool(true));
        deny_unknown("test", &opts, &["bar", "baz"]).expect("ok");
    }

    #[test]
    fn fuse_config_enabled_true() {
        let yaml = "fuse:\n  enabled: true\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::On);
    }

    #[test]
    fn fuse_config_enabled_false() {
        let yaml = "fuse:\n  enabled: false\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::Off);
    }

    #[test]
    fn fuse_config_defaults_when_omitted() {
        let yaml = "fuse: {}\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::Off);
        assert!(f.is_off());
        assert!(!f.is_on());
    }

    #[test]
    fn fuse_config_rejects_unknown_field() {
        let yaml = "fuse:\n  bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn fuse_config_enabled_auto() {
        let yaml = "fuse:\n  enabled: auto\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::Auto);
        assert!(!f.is_off());
        assert!(!f.is_on());
    }

    #[test]
    fn fuse_config_enabled_rejects_unknown_string() {
        let yaml = "fuse:\n  enabled: maybe\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("maybe"), "{err}");
    }

    #[test]
    fn fuse_config_default_struct_is_off() {
        let f = FuseConfig::default();
        assert_eq!(f.mode(), FuseMode::Off);
        assert!(f.is_off());
        assert_eq!(f.backend, None);
    }

    #[test]
    fn fuse_config_backend_kext() {
        let yaml = "fuse:\n  enabled: true\n  backend: kext\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.backend, Some(FuseBackend::Kext));
        assert_eq!(f.mode(), FuseMode::On);
    }

    #[test]
    fn fuse_config_backend_fskit() {
        let yaml = "fuse:\n  enabled: true\n  backend: fskit\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.backend, Some(FuseBackend::Fskit));
    }

    #[test]
    fn fuse_config_backend_defaults_none_when_omitted() {
        let yaml = "fuse:\n  enabled: true\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.backend, None);
    }

    #[test]
    fn fuse_config_backend_rejects_unknown() {
        let yaml = "fuse:\n  enabled: true\n  backend: bogus\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn fuse_config_helpers_set_mode_and_backend() {
        assert_eq!(FuseConfig::on().mode(), FuseMode::On);
        assert_eq!(FuseConfig::auto().mode(), FuseMode::Auto);
        let f = FuseConfig::on().with_backend(FuseBackend::Fskit);
        assert_eq!(f.backend, Some(FuseBackend::Fskit));
        assert_eq!(f.mode(), FuseMode::On);
    }

    #[test]
    fn lock_config_backend_mem() {
        let yaml = "lock:\n  backend: mem\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let l = cfg.lock.expect("lock present");
        assert_eq!(l.backend, Some(LockBackendConfig::Mem));
    }

    #[test]
    fn lock_config_backend_fs() {
        let yaml = "lock:\n  backend: fs\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(
            cfg.lock.expect("lock present").backend,
            Some(LockBackendConfig::Fs)
        );
    }

    #[test]
    fn lock_config_omitted_is_none() {
        let yaml = "plugins: []\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        assert!(cfg.lock.is_none());
    }

    #[test]
    fn lock_config_rejects_unknown_backend() {
        let yaml = "lock:\n  backend: sqlite\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("sqlite"), "{err}");
    }

    #[test]
    fn lock_config_rejects_unknown_field() {
        let yaml = "lock:\n  bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn telemetry_defaults_enabled_when_omitted() {
        // Opt-out: no `telemetry:` block means telemetry stays on.
        let cfg: ConfigYaml = serde_yaml::from_str("plugins: []\n").expect("parse");
        assert!(cfg.telemetry.is_none());
        assert!(cfg.telemetry_enabled());
    }

    #[test]
    fn telemetry_can_be_disabled() {
        let cfg: ConfigYaml =
            serde_yaml::from_str("telemetry:\n  enabled: false\n").expect("parse");
        assert!(!cfg.telemetry_enabled());
    }

    #[test]
    fn telemetry_empty_block_defaults_enabled() {
        // `telemetry: {}` present but `enabled` omitted → still on.
        let cfg: ConfigYaml = serde_yaml::from_str("telemetry: {}\n").expect("parse");
        assert!(cfg.telemetry_enabled());
    }

    #[test]
    fn telemetry_rejects_unknown_field() {
        let yaml = "telemetry:\n  bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigYaml>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn merge_overrides_caches_by_key() {
        let mut base: ConfigYaml = serde_yaml::from_str(
            "caches:\n  a:\n    uri: s3://base/a\n  b:\n    uri: s3://base/b\n",
        )
        .expect("parse base");
        let overlay: ConfigYaml = serde_yaml::from_str(
            "caches:\n  b:\n    uri: s3://prof/b\n  c:\n    uri: s3://prof/c\n",
        )
        .expect("parse overlay");

        base.merge(overlay);

        let caches = base.resolved_caches().expect("resolve");
        assert_eq!(caches.len(), 3);
        // Untouched key keeps its base value.
        assert_eq!(caches.get("a").expect("a").uri, "s3://base/a");
        // Shared key's uri is overridden by the overlay.
        assert_eq!(caches.get("b").expect("b").uri, "s3://prof/b");
        // New key is added.
        assert_eq!(caches.get("c").expect("c").uri, "s3://prof/c");
    }

    #[test]
    fn merge_deep_merges_cache_fields() {
        // Base defines uri (+ default read/write); profile flips write only.
        let mut base: ConfigYaml =
            serde_yaml::from_str("caches:\n  remote:\n    uri: s3://base/remote\n")
                .expect("parse base");
        let overlay: ConfigYaml =
            serde_yaml::from_str("caches:\n  remote:\n    write: false\n").expect("parse overlay");

        base.merge(overlay);

        let remote = base
            .resolved_caches()
            .expect("resolve")
            .remove("remote")
            .expect("remote present");
        // uri inherited from base, write patched by profile, read still default.
        assert_eq!(remote.uri, "s3://base/remote");
        assert!(remote.read);
        assert!(!remote.write);
    }

    #[test]
    fn merge_overrides_scalars_only_when_present() {
        let mut base: ConfigYaml =
            serde_yaml::from_str("homeDir: .base\nlock:\n  backend: fs\n").expect("parse base");
        // Overlay sets homeDir but omits lock — lock must survive.
        let overlay: ConfigYaml = serde_yaml::from_str("homeDir: .prof\n").expect("parse overlay");

        base.merge(overlay);

        assert_eq!(base.home_dir.as_deref(), Some(Path::new(".prof")));
        assert_eq!(
            base.lock.expect("lock survives").backend,
            Some(LockBackendConfig::Fs)
        );
    }

    #[test]
    fn merge_plugins_by_identity() {
        let mut base: ConfigYaml = serde_yaml::from_str(
            "plugins:\n  - builtin: buildfile\n    options:\n      patterns: [BUILD]\n  - builtin: go\n",
        )
        .expect("parse base");
        let overlay: ConfigYaml = serde_yaml::from_str(
            "plugins:\n  - builtin: buildfile\n    options:\n      patterns: [BUILD2]\n  - builtin: rust\n",
        )
        .expect("parse overlay");

        base.merge(overlay);

        // buildfile replaced, go kept, rust appended — order preserved.
        let ids: Vec<PluginIdentifier> =
            base.plugins.iter().map(|p| p.identifier.clone()).collect();
        assert_eq!(
            ids,
            vec![
                PluginIdentifier::Builtin("buildfile".into()),
                PluginIdentifier::Builtin("go".into()),
                PluginIdentifier::Builtin("rust".into()),
            ]
        );
        let patterns: Vec<String> = decode_opt(&base.plugins[0].options, "buildfile", "patterns")
            .expect("decode")
            .expect("present");
        assert_eq!(patterns, vec!["BUILD2".to_string()]);
    }

    #[test]
    fn load_with_profiles_layers_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join(".hephconfig"),
            "homeDir: .base\ncaches:\n  shared:\n    uri: s3://base/shared\n",
        )
        .expect("write base");
        std::fs::write(
            root.join("a.hephconfig"),
            "caches:\n  shared:\n    uri: s3://a/shared\n",
        )
        .expect("write a");
        std::fs::write(
            root.join("b.hephconfig"),
            "homeDir: .b\ncaches:\n  shared:\n    uri: s3://b/shared\n",
        )
        .expect("write b");

        let cfg = load_with_profiles(root, &["a", "b"]).expect("load");

        // Last profile wins per key / scalar.
        assert_eq!(cfg.home_dir.as_deref(), Some(Path::new(".b")));
        let caches = cfg.resolved_caches().expect("resolve");
        assert_eq!(caches.get("shared").expect("shared").uri, "s3://b/shared");
    }

    #[test]
    fn load_with_profiles_no_profiles_is_base() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".hephconfig"), "homeDir: .base\n").expect("write base");

        let cfg = load_with_profiles(root, &[] as &[&str]).expect("load");
        assert_eq!(cfg.home_dir.as_deref(), Some(Path::new(".base")));
    }

    #[test]
    fn load_with_profiles_missing_profile_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".hephconfig"), "homeDir: .base\n").expect("write base");

        let err = load_with_profiles(root, &["missing"]).expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing.hephconfig"), "{msg}");
    }

    #[test]
    fn version_layered_by_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".hephconfig"), "version: v1.0.0\n").expect("write base");
        std::fs::write(root.join("ci.hephconfig"), "version: v2.0.0\n").expect("write profile");

        let cfg = load_with_profiles(root, &["ci"]).expect("load");
        assert_eq!(cfg.version.as_deref(), Some("v2.0.0"));
    }

    #[test]
    fn config_file_name_prefers_legacy_then_canonical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // Neither present: fall back to the canonical name for error paths.
        assert_eq!(config_file_name_at(root), ".hephconfig");

        // Only canonical present: pick it.
        std::fs::write(root.join(".hephconfig"), "homeDir: .canonical\n").expect("write canonical");
        assert_eq!(config_file_name_at(root), ".hephconfig");

        // Both present: the migration `.hephconfig2` wins.
        std::fs::write(root.join(".hephconfig2"), "homeDir: .legacy\n").expect("write legacy");
        assert_eq!(config_file_name_at(root), ".hephconfig2");
    }

    #[test]
    fn load_from_canonical_config_when_legacy_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".hephconfig"), "homeDir: .canonical\n").expect("write canonical");

        let cfg = load_with_profiles(root, &[] as &[&str]).expect("load");
        assert_eq!(cfg.home_dir.as_deref(), Some(Path::new(".canonical")));
    }

    #[test]
    fn load_prefers_legacy_when_both_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".hephconfig"), "homeDir: .canonical\n").expect("write canonical");
        std::fs::write(root.join(".hephconfig2"), "homeDir: .legacy\n").expect("write legacy");

        let cfg = load_with_profiles(root, &[] as &[&str]).expect("load");
        assert_eq!(cfg.home_dir.as_deref(), Some(Path::new(".legacy")));
    }

    #[test]
    fn legacy_base_uses_legacy_profile_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".hephconfig2"), "homeDir: .base\n").expect("write base");
        // Profile must follow the chosen base name: `.hephconfig2`, not `.hephconfig`.
        std::fs::write(root.join("a.hephconfig2"), "homeDir: .a\n").expect("write a");

        let cfg = load_with_profiles(root, &["a"]).expect("load");
        assert_eq!(cfg.home_dir.as_deref(), Some(Path::new(".a")));
    }

    #[test]
    fn parses_plugin_identifier_variants() {
        let yaml = r#"
plugins:
  - builtin: buildfile
    options: { patterns: [BUILD] }
  - path: .heph3/heph-go-plugin.json
  - url: https://example.com/heph-go-plugin.json
"#;
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(
            cfg.plugins[0].identifier,
            PluginIdentifier::Builtin("buildfile".into())
        );
        assert_eq!(
            cfg.plugins[1].identifier,
            PluginIdentifier::Path(".heph3/heph-go-plugin.json".into())
        );
        assert_eq!(
            cfg.plugins[2].identifier,
            PluginIdentifier::Url("https://example.com/heph-go-plugin.json".into())
        );
    }

    #[test]
    fn plugin_parses_url_checksum() {
        let yaml = "plugins:\n  - url: https://e/x.json\n    checksum: sha256:abc123\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.plugins[0].checksum.as_deref(), Some("sha256:abc123"));
    }

    #[test]
    fn plugin_checksum_defaults_none() {
        let yaml = "plugins:\n  - url: https://e/x.json\n";
        let cfg: ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        assert!(cfg.plugins[0].checksum.is_none());
    }

    #[test]
    fn plugin_rejects_checksum_without_url() {
        // checksum pins a downloaded manifest, so it is only valid with `url:`.
        let err = serde_yaml::from_str::<ConfigYaml>(
            "plugins:\n  - path: ./x.json\n    checksum: sha256:abc\n",
        )
        .expect_err("must reject");
        assert!(
            err.to_string().contains("only valid with a `url:`"),
            "{err}"
        );
    }

    #[test]
    fn plugin_rejects_multiple_source_keys() {
        // Exactly one of builtin/path/url: two sources is a deserialize error.
        let err = serde_yaml::from_str::<ConfigYaml>("plugins:\n  - builtin: go\n    path: /a\n")
            .expect_err("must reject");
        assert!(err.to_string().contains("exactly one"), "{err}");
    }

    #[test]
    fn plugin_rejects_missing_source() {
        let err = serde_yaml::from_str::<ConfigYaml>("plugins:\n  - options: { x: 1 }\n")
            .expect_err("must reject");
        assert!(err.to_string().contains("builtin/path/url"), "{err}");
    }

    #[test]
    fn plugin_rejects_unknown_field() {
        let err = serde_yaml::from_str::<ConfigYaml>("plugins:\n  - bogus: x\n")
            .expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }
}
