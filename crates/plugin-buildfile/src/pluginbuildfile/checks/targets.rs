//! The three check targets every package with a BUILD file gets: their names,
//! labels and drivers (what `list` reports without building a spec), the
//! option that configures them, and the specs `get` builds.
//!
//! Like the Go plugin's `listed.rs`, a spec builder never spells a label or a
//! driver inline: both come from [`CheckKind`], and so do the listed facts, so
//! `check_listed_facts_equal_spec_and_def` holds them to one source.

use super::loads::LoadWalk;
use anyhow::Context;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::{ListedFacts, ProvenanceFrame, TargetSpec};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

/// The plugin the check drivers belong to; a driver's full name is
/// `<plugin>.<local>`.
const PLUGIN: &str = "buildfile";

/// One of the three check targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CheckKind {
    /// `build-lint-check`: the static typecheck, plus annotations when
    /// `requireTypeAnnotations` is on.
    LintCheck,
    /// `build-format-check`: fails on an owned file that is not formatted.
    FormatCheck,
    /// `build-format`: rewrites the owned files, `codegen = in_place`.
    Format,
}

impl CheckKind {
    /// Every kind, in the order `list` emits them.
    pub(crate) const ALL: [CheckKind; 3] = [Self::LintCheck, Self::FormatCheck, Self::Format];

    /// The target name in every package.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::LintCheck => "build-lint-check",
            Self::FormatCheck => "build-format-check",
            Self::Format => "build-format",
        }
    }

    /// The kind a target name is, if it is one.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// The labels, mirroring the Go plugin's: the fixer owns `format` and
    /// `fix`, the read-only checks the `*-check` labels.
    pub(crate) fn labels(self) -> &'static [&'static str] {
        match self {
            Self::LintCheck => &["build-lint-check", "lint-check"],
            Self::FormatCheck => &["build-format-check", "format-check"],
            Self::Format => &["build-format", "format", "fix"],
        }
    }

    /// The driver's local name within the `buildfile` plugin.
    pub(crate) fn driver_local(self) -> &'static str {
        match self {
            Self::LintCheck => "lint_check",
            Self::FormatCheck => "format_check",
            Self::Format => "format",
        }
    }

    /// The driver's full name, as a spec names it.
    pub(crate) fn driver(self) -> String {
        format!("{PLUGIN}.{}", self.driver_local())
    }

    /// Only `build-format` writes into the tree (`in_place`).
    pub(crate) fn has_codegen(self) -> bool {
        matches!(self, Self::Format)
    }

    /// What `list` reports for this kind without building its spec. Every
    /// field is known, and held to the spec and def by a test.
    pub(crate) fn facts(self) -> ListedFacts {
        ListedFacts::default()
            .with_labels(self.labels().iter().copied())
            .with_driver(self.driver())
            .with_has_codegen(self.has_codegen())
    }
}

/// The check kinds a package's own targets shadow (I2): a target declared
/// with a check's name, in the BUILD file or by a plugin function, replaces
/// that check in `list` and `get`. `heph validate` reports each one; this is
/// the one place that decides which names shadow.
pub(crate) fn shadowed<'a>(
    target_names: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'static str> {
    target_names
        .into_iter()
        .filter_map(CheckKind::from_name)
        .map(CheckKind::name)
        .collect()
}

/// A target that shadows a check target (I2), as `heph validate` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedCheck {
    /// The user's target, which `list` and `get` answer for the check's name.
    pub addr: Addr,
    /// Where it was declared: the innermost call site (`target()`, or the
    /// plugin function's call), and the outermost one when that is elsewhere
    /// (a macro in a loaded file, called from the BUILD file). `None` only if
    /// evaluation recorded no call site.
    pub declared_at: Option<String>,
    /// The check it shadows, its target name.
    pub check: &'static str,
}

impl std::fmt::Display for ShadowedCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.addr.format())?;
        if let Some(at) = &self.declared_at {
            write!(f, ", declared at {at},")?;
        }
        write!(
            f,
            " shadows the BUILD-file check `{}`: rename the target, or remove it to get the check back",
            self.check
        )
    }
}

/// The [`ShadowedCheck`]s of `pkg`'s targets, given each target's name and
/// call-site provenance (outermost frame first, as evaluation captures it).
/// Paths are shown relative to `root`.
pub(crate) fn shadowed_checks<'a>(
    root: &std::path::Path,
    pkg: &hmodel::htpkg::PkgBuf,
    targets: impl IntoIterator<Item = (&'a str, &'a [ProvenanceFrame])>,
) -> Vec<ShadowedCheck> {
    let site = |f: &ProvenanceFrame| {
        let file = std::path::Path::new(&f.file);
        let shown = file.strip_prefix(root).unwrap_or(file);
        format!("{}:{}", shown.display(), f.line_start)
    };
    targets
        .into_iter()
        .filter_map(|(name, provenance)| {
            let check = CheckKind::from_name(name)?;
            let declared_at = match (provenance.last(), provenance.first()) {
                (Some(inner), Some(outer)) if inner != outer => {
                    Some(format!("{} (called from {})", site(inner), site(outer)))
                }
                (Some(inner), _) => Some(site(inner)),
                (None, _) => None,
            };
            Some(ShadowedCheck {
                addr: Addr::new(pkg.clone(), name.to_string(), Default::default()),
                declared_at,
                check: check.name(),
            })
        })
        .collect()
}

/// Every [`ShadowedCheck`] the provider found, per package, recorded as it
/// lists each one. A cheap handle: clones share the record. `heph validate`
/// reads it after its walks have listed every package in scope; nothing else
/// does, so `heph run` and `query` stay quiet about a deliberate shadow.
#[derive(Debug, Clone, Default)]
pub struct ShadowedChecks(Arc<Mutex<BTreeMap<String, Vec<ShadowedCheck>>>>);

impl ShadowedChecks {
    /// Replace what is recorded for `pkg`: a package listed again (a later
    /// request, a fixed BUILD file) never reports a stale conflict twice.
    pub(crate) fn record(&self, pkg: &str, found: Vec<ShadowedCheck>) {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if found.is_empty() {
            map.remove(pkg);
        } else {
            map.insert(pkg.to_string(), found);
        }
    }

    /// Every conflict recorded, sorted by package, then in the package's
    /// target order.
    pub fn all(&self) -> Vec<ShadowedCheck> {
        let map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        map.values().flatten().cloned().collect()
    }
}

/// The buildfile options the checks read, decoded once by `plugin_parts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChecksOptions {
    /// `requireTypeAnnotations`: every `def` parameter must be annotated.
    pub require_type_annotations: bool,
    /// `indent`: the formatter's indentation width.
    pub indent: usize,
}

impl Default for ChecksOptions {
    fn default() -> Self {
        Self {
            require_type_annotations: false,
            indent: crate::pluginbuildfile::DEFAULT_INDENT,
        }
    }
}

impl ChecksOptions {
    /// Decode from the buildfile plugin's `options:`. A value of the wrong
    /// type is refused, naming the option.
    pub fn from_options(opts: &hplugin::config::Options) -> anyhow::Result<Self> {
        let require_type_annotations =
            hplugin::config::decode_opt(opts, "buildfile provider", "requireTypeAnnotations")?
                .unwrap_or(false);
        let indent = crate::pluginbuildfile::build_file_indent_from_options(opts)?;
        Ok(Self {
            require_type_annotations,
            indent,
        })
    }
}

/// `build-lint-check`'s spec config keys.
pub(crate) mod lint_keys {
    pub(crate) const WALK: &str = "walk";
    pub(crate) const REQUIRE: &str = "requireTypeAnnotations";
    pub(crate) const PATTERNS: &str = "patterns";
    pub(crate) const PLUGIN_FNS_DIGEST: &str = "pluginFnsDigest";
}

/// One digest of the `(plugin, fn)` names the checker knows, sorted and
/// deduplicated. The lint spec carries it instead of the names, so a spec
/// costs the same however many functions the plugins export; the driver
/// reads the names themselves from the plugin function registry and checks
/// them against it.
///
/// Plugin functions are hashed by name only: the checker sees each as an
/// opaque callable taking and returning anything, so a function's signature
/// cannot move the verdict, only whether `heph.<plugin>.<fn>` exists.
pub(crate) fn plugin_fns_digest<'a>(names: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    use std::hash::Hash;
    let names: BTreeSet<(&str, &str)> = names.into_iter().collect();
    let mut h = xxhash_rust::xxh3::Xxh3Default::new();
    names.hash(&mut h);
    format!("{:032x}", h.digest128())
}

/// The format kinds' spec config keys.
pub(crate) mod format_keys {
    pub(crate) const FILES: &str = "files";
    pub(crate) const INDENT: &str = "indent";
}

fn strings(items: impl IntoIterator<Item = impl Into<String>>) -> Value {
    Value::List(items.into_iter().map(|s| Value::String(s.into())).collect())
}

fn spec(addr: Addr, kind: CheckKind, config: HashMap<String, Value>) -> TargetSpec {
    TargetSpec {
        addr,
        driver: kind.driver(),
        config,
        labels: kind.labels().iter().map(|l| (*l).to_string()).collect(),
        transitive: Default::default(),
        approval: Default::default(),
    }
}

/// `build-lint-check`'s spec: the package's load walk (its edges and every
/// file it reads), the option, the BUILD patterns and the
/// [digest](plugin_fns_digest) of the plugin function names the checker
/// knows.
pub(crate) fn lint_spec<'a>(
    addr: Addr,
    walk: &LoadWalk,
    opts: ChecksOptions,
    patterns: &[glob::Pattern],
    plugin_fns: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> anyhow::Result<TargetSpec> {
    let walk = serde_json::to_string(walk).context("encoding the load walk")?;
    let config = HashMap::from([
        (lint_keys::WALK.to_string(), Value::String(walk)),
        (
            lint_keys::REQUIRE.to_string(),
            Value::Bool(opts.require_type_annotations),
        ),
        (
            lint_keys::PATTERNS.to_string(),
            strings(patterns.iter().map(glob::Pattern::as_str)),
        ),
        (
            lint_keys::PLUGIN_FNS_DIGEST.to_string(),
            Value::String(plugin_fns_digest(plugin_fns)),
        ),
    ]);
    Ok(spec(addr, CheckKind::LintCheck, config))
}

/// A format kind's spec: the package's owned files and the indent.
pub(crate) fn format_spec(
    addr: Addr,
    kind: CheckKind,
    walk: &LoadWalk,
    opts: ChecksOptions,
) -> TargetSpec {
    let config = HashMap::from([
        (format_keys::FILES.to_string(), strings(walk.owned())),
        (
            format_keys::INDENT.to_string(),
            Value::Uint(opts.indent as u64),
        ),
    ]);
    spec(addr, kind, config)
}
