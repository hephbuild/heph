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
    /// Extra workspace-relative glob patterns the `fs` plugin excludes from every
    /// glob walk, on top of the always-skipped `.git` and engine-owned dirs.
    /// Matched against file paths like a target's own `exclude`.
    #[serde(default)]
    pub skip: Vec<String>,
    #[serde(default)]
    pub providers: Vec<PluginEntry>,
    #[serde(default)]
    pub drivers: Vec<PluginEntry>,
    #[serde(default)]
    pub mem_cache: Option<MemCacheConfig>,
    #[serde(default)]
    pub fuse: Option<FuseConfig>,
    #[serde(default)]
    pub lock: Option<LockConfig>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MemCacheConfig {
    pub per_entry_bytes: usize,
    /// Total byte budget for the in-memory cache. `0` disables it entirely.
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
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].name, "buildfile");
        let patterns: Vec<String> = decode_opt(&cfg.providers[0].options, "buildfile", "patterns")
            .expect("decode")
            .expect("present");
        assert_eq!(patterns, vec!["BUILD2".to_string(), "*.BUILD2".to_string()]);
        assert_eq!(cfg.drivers.len(), 2);
    }

    #[test]
    fn parses_top_level_skip() {
        let yaml = "skip:\n  - vendor/**\n  - \"**/*.tmp\"\n";
        let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(
            cfg.skip,
            vec!["vendor/**".to_string(), "**/*.tmp".to_string()]
        );
    }

    #[test]
    fn skip_defaults_empty() {
        let cfg: ConfigFile = serde_yaml::from_str("homeDir: .heph3\n").expect("parse");
        assert!(cfg.skip.is_empty());
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
}
