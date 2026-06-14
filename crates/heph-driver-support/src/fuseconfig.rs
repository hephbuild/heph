//! FUSE-overlay mode config, used by the engine (parsed from YAML) and the
//! managed-driver bridge (per-target FUSE-vs-copy decision). Lives here so the
//! bridge can read it without depending on the engine's config module.

use serde::Deserialize;
use serde::de::{Error as DeError, Visitor};
use std::fmt;

/// Sandbox FUSE-overlay mode. `fuse: { enabled: true | false | auto }` selects
/// mode explicitly. Omit `enabled` (or the entire `fuse:` block) to default off.
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
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
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
