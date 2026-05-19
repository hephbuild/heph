use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
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
    serde_yaml::from_slice(&bytes)
        .with_context(|| format!("parsing YAML config {}", path.display()))
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
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].name, "buildfile");
        let patterns: Vec<String> = decode_opt(&cfg.providers[0].options, "buildfile", "patterns")
            .expect("decode")
            .expect("present");
        assert_eq!(patterns, vec!["BUILD2".to_string(), "*.BUILD2".to_string()]);
        assert_eq!(cfg.drivers.len(), 2);
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
}
