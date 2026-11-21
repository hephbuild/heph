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
use hplugin::provider::{ListedFacts, TargetSpec};
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    pub(crate) const PLUGIN_FNS: &str = "pluginFns";
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
/// file it reads), the option, the BUILD patterns and the plugin function
/// names the checker knows.
pub(crate) fn lint_spec<'a>(
    addr: Addr,
    walk: &LoadWalk,
    opts: ChecksOptions,
    patterns: &[glob::Pattern],
    plugin_fns: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> anyhow::Result<TargetSpec> {
    let walk = serde_json::to_string(walk).context("encoding the load walk")?;
    let mut fns: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (plugin, func) in plugin_fns {
        fns.entry(plugin).or_default().insert(func);
    }
    let fns = fns
        .into_iter()
        .map(|(p, f)| (p.to_string(), strings(f)))
        .collect();
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
        (lint_keys::PLUGIN_FNS.to_string(), Value::Map(fns)),
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
