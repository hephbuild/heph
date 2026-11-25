//! What a failed check prints: one line per finding, sorted by file, line and
//! column, each with a stable prefix and the edit that fixes it, then a count
//! line and the footer for the kind of check. The text is the contract
//! (docs/BUILDFILE_CHECKS.md); `checks::failure_format` pins it. The count
//! line is also returned on its own, as the run's error.

use std::fmt;

/// The kind of a finding, which is its stable line prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    /// A `def` parameter without a type, under `requireTypeAnnotations`.
    Annotation,
    /// An error from Starlark's static typechecker.
    TypeError,
    /// A file that differs from the formatter's output.
    Format,
}

impl Kind {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Kind::Annotation => "annotation",
            Kind::TypeError => "type-error",
            Kind::Format => "format",
        }
    }
}

/// One finding at a position in a file. Ordered by file, line and column (the
/// field order), so sorting a list sorts the report.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Finding {
    /// Workspace-relative path.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column, counted in characters, as an editor counts.
    pub col: usize,
    pub kind: Kind,
    /// What is wrong and, where there is one, the edit that fixes it.
    pub message: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}: {}",
            self.file,
            self.line,
            self.col,
            self.kind.prefix(),
            self.message
        )
    }
}

/// Where a record or enum a `type-error` finding mentions was declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclarationSite {
    /// The type's name, as the message prints it (`Lib2 = Lib` prints `Lib`).
    pub name: String,
    /// The declaring file's workspace-relative path.
    pub file: String,
    /// 1-based line of the binding that declared it.
    pub line: usize,
}

/// How many declaration sites a finding names before `and N more`.
const MAX_SITES: usize = 3;

impl Finding {
    /// Append where the types this finding mentions were declared, in the
    /// order given (the message's): ` (Lib declared at lib/defs.star:3)`, or
    /// ` (A declared at a.star:3; B declared at b.star:7)`. A site already
    /// named (same file and line) is skipped; past [`MAX_SITES`], the rest are
    /// counted as `; and N more`. No site leaves the message unchanged.
    ///
    /// The suffix goes at the end of the message, which is the finding's only
    /// line, so its prefix and starlark's message stem stay as they were.
    pub(crate) fn name_declarations<'a>(
        &mut self,
        sites: impl IntoIterator<Item = &'a DeclarationSite>,
    ) {
        let mut distinct: Vec<&DeclarationSite> = Vec::new();
        for site in sites {
            if !distinct
                .iter()
                .any(|seen| seen.file == site.file && seen.line == site.line)
            {
                distinct.push(site);
            }
        }
        if distinct.is_empty() {
            return;
        }
        let named = distinct.len().min(MAX_SITES);
        self.message.push_str(" (");
        for (i, site) in distinct.iter().take(named).enumerate() {
            if i > 0 {
                self.message.push_str("; ");
            }
            let DeclarationSite { name, file, line } = site;
            self.message
                .push_str(&format!("{name} declared at {file}:{line}"));
        }
        let more = distinct.len() - named;
        if more > 0 {
            self.message.push_str(&format!("; and {more} more"));
        }
        self.message.push(')');
    }
}

/// The last line of a failure that has an `annotation:` finding: where the
/// rule it breaks is switched on.
pub(crate) const ANNOTATIONS_FOOTER: &str = "annotations are required by requireTypeAnnotations in .hephconfig (plugins: buildfile, options)";

/// A failed check: the text for its failure log, and the one line that is the
/// run's error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    /// The failure log: one line per finding, the count line, the footer.
    pub log: String,
    /// The run's error: the count line (`N findings in //pkg`), or for a run
    /// that could not finish, a line naming why.
    pub summary: String,
}

/// `build-lint-check`'s failure for `pkg`, or `None` when there is no
/// finding.
pub(crate) fn lint_failure(pkg: &str, findings: &[Finding]) -> Option<Failure> {
    let footer = findings
        .iter()
        .any(|f| f.kind == Kind::Annotation)
        .then_some(ANNOTATIONS_FOOTER);
    failure(pkg, findings, footer)
}

/// `build-format-check`'s failure for `pkg`, or `None` when every file is
/// formatted.
pub(crate) fn format_failure(pkg: &str, findings: &[Finding]) -> Option<Failure> {
    let footer = format!("run heph run //{pkg}:build-format to fix");
    failure(pkg, findings, Some(&footer))
}

fn failure(pkg: &str, findings: &[Finding], footer: Option<&str>) -> Option<Failure> {
    if findings.is_empty() {
        return None;
    }
    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort();
    let mut log = String::new();
    for finding in sorted {
        log.push_str(&finding.to_string());
        log.push('\n');
    }
    let n = findings.len();
    let noun = if n == 1 { "finding" } else { "findings" };
    let summary = format!("{n} {noun} in //{pkg}");
    log.push_str(&summary);
    if let Some(footer) = footer {
        log.push('\n');
        log.push_str(footer);
    }
    Some(Failure { log, summary })
}
