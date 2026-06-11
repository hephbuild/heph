use anyhow::Context;
use serde::Deserialize;
use serde::de::{Deserializer, Error as DeError, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub type Options = BTreeMap<String, serde_yaml::Value>;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConfigFile {
    #[serde(default)]
    pub home_dir: Option<PathBuf>,
    #[serde(default)]
    pub providers: Vec<PluginEntry>,
    #[serde(default)]
    pub drivers: Vec<PluginEntry>,
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
    /// `uri` plus `read`/`write` permissions. See [`RemoteCacheConfig`].
    #[serde(default)]
    pub caches: BTreeMap<String, RemoteCacheConfig>,
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
}

/// One named remote cache: `caches: { name: { uri, read, write } }`.
///
/// `uri` selects the backend by scheme — `s3://bucket/prefix`,
/// `gs://bucket/prefix` (credentials come from the environment, e.g.
/// `AWS_ACCESS_KEY_ID` / `GOOGLE_SERVICE_ACCOUNT`), plus `memory://` and
/// `file://` for tests/local use. `read`/`write` gate whether the cache is
/// consulted on lookups and pushed to on writes; both default to `true`.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteCacheConfig {
    pub uri: String,
    #[serde(default = "default_true")]
    pub read: bool,
    #[serde(default = "default_true")]
    pub write: bool,
}

fn default_true() -> bool {
    true
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

impl ConfigFile {
    /// Whether anonymous usage telemetry is enabled. Opt-out: enabled unless the
    /// config explicitly sets `telemetry.enabled: false`.
    pub fn telemetry_enabled(&self) -> bool {
        self.telemetry.map(|t| t.enabled).unwrap_or(true)
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

/// Sandbox FUSE-overlay mode. `fuse: { enabled: true | false | auto }`
/// selects mode explicitly. Omit `enabled` (or the entire `fuse:` block) to
/// default to off; FUSE is opt-in.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FuseConfig {
    #[serde(default)]
    pub enabled: Option<FuseEnabled>,
}

/// Tri-state config value. Parses YAML `true`, `false`, or `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseEnabled {
    On,
    Off,
    Auto,
}

impl<'de> Deserialize<'de> for FuseEnabled {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
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

/// Resolved decision used by Engine + bridge. Independent of YAML shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseMode {
    /// Forced on. Probe must succeed; mount errors propagate.
    On,
    /// Forced off.
    Off,
    /// Engine decides per-target by walking inputs.
    Auto,
}

impl FuseConfig {
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    pub name: String,
    #[serde(default)]
    pub options: Options,
}

/// Filename of the per-workspace config, located at the repo root. The single
/// source of truth for this name — [`load_from_root`] and the root discovery in
/// [`crate::engine::get_root`] both build on it.
pub const CONFIG_FILE_NAME: &str = ".hephconfig2";

/// Load the workspace config from `<root>/.hephconfig2`.
pub fn load_from_root(root: &Path) -> anyhow::Result<ConfigFile> {
    load(&root.join(CONFIG_FILE_NAME))
}

pub fn load(path: &Path) -> anyhow::Result<ConfigFile> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading config file {}", path.display()))?;
    if bytes.is_empty() {
        return Ok(ConfigFile::default());
    }
    let cfg: ConfigFile = serde_yaml::from_slice(&bytes)
        .with_context(|| format!("parsing YAML config {}", path.display()))?;
    Ok(cfg)
}

/// Decode a single option value into `T`. Returns the default if the key is absent.
pub fn decode_opt<T: for<'de> Deserialize<'de>>(
    opts: &Options,
    plugin: &str,
    key: &str,
) -> anyhow::Result<Option<T>> {
    match opts.get(key) {
        Some(v) => {
            let parsed = serde_yaml::from_value::<T>(v.clone())
                .with_context(|| format!("{plugin}: invalid value for option `{key}`"))?;
            Ok(Some(parsed))
        }
        None => Ok(None),
    }
}

/// Verify that `opts` contains only the keys listed in `allowed`. Errors otherwise.
pub fn deny_unknown(plugin: &str, opts: &Options, allowed: &[&str]) -> anyhow::Result<()> {
    for key in opts.keys() {
        if !allowed.iter().any(|k| k == key) {
            anyhow::bail!(
                "{plugin}: unknown option `{key}` (allowed: {})",
                allowed.join(", ")
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
providers:
  - name: buildfile
    options:
      patterns:
        - BUILD2
        - "*.BUILD2"
  - name: go
    options:
      gotool: //tools/heph:go
drivers:
  - name: exec
    options:
      path:
        - /usr/sbin
        - /usr/bin
  - name: bash
    options:
      path: [/usr/bin]
"#;
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.home_dir.as_deref(), Some(Path::new(".heph2")));
        let mc = cfg.mem_cache.expect("mem_cache present");
        assert_eq!(mc.per_entry_bytes, 16384);
        assert_eq!(mc.capacity_bytes, 67108864);
        let tc = cfg.tmp_cache.expect("tmp_cache present");
        assert_eq!(tc.per_entry_bytes, 1048576);
        assert_eq!(tc.capacity_bytes, 134217728);
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].name, "buildfile");
        let patterns: Vec<String> = decode_opt(&cfg.providers[0].options, "buildfile", "patterns")
            .expect("decode")
            .expect("present");
        assert_eq!(patterns, vec!["BUILD2".to_string(), "*.BUILD2".to_string()]);
        assert_eq!(cfg.drivers.len(), 2);
    }

    #[test]
    fn parses_cache_spill_threshold() {
        let yaml = "cache:\n  spillThresholdBytes: 104857600\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        let c = cfg.cache.expect("cache present");
        assert_eq!(c.spill_threshold_bytes, Some(104857600));
    }

    #[test]
    fn cache_config_omitted_is_none() {
        let cfg: ConfigFile = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.cache.is_none());
    }

    #[test]
    fn rejects_unknown_cache_field() {
        let err = serde_yaml::from_str::<ConfigFile>("cache:\n  bogus: 1\n").expect_err("reject");
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
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.caches.len(), 2);
        let remote = cfg.caches.get("remote").expect("remote present");
        assert_eq!(remote.uri, "s3://my-bucket/heph");
        assert!(remote.read);
        assert!(!remote.write);
        // read/write default to true when omitted.
        let local = cfg.caches.get("local").expect("local present");
        assert!(local.read);
        assert!(local.write);
    }

    #[test]
    fn caches_omitted_is_empty() {
        let cfg: ConfigFile = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.caches.is_empty());
    }

    #[test]
    fn rejects_unknown_cache_entry_field() {
        let yaml = "caches:\n  r:\n    uri: memory:///r\n    bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigFile>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn parses_fs_skip() {
        // `skip` mixes literal dirs and glob patterns.
        let yaml = "fs:\n  skip: [vendor, \"**/node_modules/**\"]\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        let fs = cfg.fs.expect("fs config present");
        assert_eq!(
            fs.skip,
            vec!["vendor".to_string(), "**/node_modules/**".to_string()]
        );
    }

    #[test]
    fn fs_config_defaults_absent() {
        let cfg: ConfigFile = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.fs.is_none());
    }

    #[test]
    fn rejects_unknown_fs_field() {
        let err = serde_yaml::from_str::<ConfigFile>("fs:\n  bogus: 1\n").expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn empty_file_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".hephconfig2");
        std::fs::write(&path, b"").expect("write");
        let cfg = load(&path).expect("load");
        assert!(cfg.home_dir.is_none());
        assert!(cfg.providers.is_empty());
        assert!(cfg.drivers.is_empty());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let yaml = "bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigFile>(yaml).expect_err("must reject");
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
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::On);
    }

    #[test]
    fn fuse_config_enabled_false() {
        let yaml = "fuse:\n  enabled: false\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::Off);
    }

    #[test]
    fn fuse_config_defaults_when_omitted() {
        let yaml = "fuse: {}\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::Off);
        assert!(f.is_off());
        assert!(!f.is_on());
    }

    #[test]
    fn fuse_config_rejects_unknown_field() {
        let yaml = "fuse:\n  bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigFile>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn fuse_config_enabled_auto() {
        let yaml = "fuse:\n  enabled: auto\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        let f = cfg.fuse.expect("fuse present");
        assert_eq!(f.mode(), FuseMode::Auto);
        assert!(!f.is_off());
        assert!(!f.is_on());
    }

    #[test]
    fn fuse_config_enabled_rejects_unknown_string() {
        let yaml = "fuse:\n  enabled: maybe\n";
        let err = serde_yaml::from_str::<ConfigFile>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("maybe"), "{err}");
    }

    #[test]
    fn fuse_config_default_struct_is_off() {
        let f = FuseConfig::default();
        assert_eq!(f.mode(), FuseMode::Off);
        assert!(f.is_off());
    }

    #[test]
    fn lock_config_backend_mem() {
        let yaml = "lock:\n  backend: mem\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        let l = cfg.lock.expect("lock present");
        assert_eq!(l.backend, Some(LockBackendConfig::Mem));
    }

    #[test]
    fn lock_config_backend_fs() {
        let yaml = "lock:\n  backend: fs\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(
            cfg.lock.expect("lock present").backend,
            Some(LockBackendConfig::Fs)
        );
    }

    #[test]
    fn lock_config_omitted_is_none() {
        let yaml = "providers: []\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        assert!(cfg.lock.is_none());
    }

    #[test]
    fn lock_config_rejects_unknown_backend() {
        let yaml = "lock:\n  backend: sqlite\n";
        let err = serde_yaml::from_str::<ConfigFile>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("sqlite"), "{err}");
    }

    #[test]
    fn lock_config_rejects_unknown_field() {
        let yaml = "lock:\n  bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigFile>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn telemetry_defaults_enabled_when_omitted() {
        // Opt-out: no `telemetry:` block means telemetry stays on.
        let cfg: ConfigFile = serde_yaml::from_str("providers: []\n").expect("parse");
        assert!(cfg.telemetry.is_none());
        assert!(cfg.telemetry_enabled());
    }

    #[test]
    fn telemetry_can_be_disabled() {
        let cfg: ConfigFile =
            serde_yaml::from_str("telemetry:\n  enabled: false\n").expect("parse");
        assert!(!cfg.telemetry_enabled());
    }

    #[test]
    fn telemetry_empty_block_defaults_enabled() {
        // `telemetry: {}` present but `enabled` omitted → still on.
        let cfg: ConfigFile = serde_yaml::from_str("telemetry: {}\n").expect("parse");
        assert!(cfg.telemetry_enabled());
    }

    #[test]
    fn telemetry_rejects_unknown_field() {
        let yaml = "telemetry:\n  bogus: 1\n";
        let err = serde_yaml::from_str::<ConfigFile>(yaml).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }
}
