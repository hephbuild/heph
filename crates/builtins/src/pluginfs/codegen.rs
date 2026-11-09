//! The fs plugin's reads of the codegen index: which files a glob, a `file`
//! target or a `copy_output` target leaves out because heph generated them,
//! and how that reaches the user.
//!
//! Every reader takes one [`Snapshot`] (J10) and asks it per path; nothing
//! here writes the index, waits on it or reads a def (J6). The fs driver
//! cannot read defs, so the one question that needs one — does the target a
//! row names still exist — is answered by the engine, on the error path only
//! (see [`CodegenFileError`]).

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use hcore::events::BuildEventKind;
use hplugin::driver::outputartifact::{Content, OutputArtifact};
use hwalk::codegen_index::{
    CodegenIndex, LookupMeta, RowState, Snapshot, Status, StatusKind, StatusReason, normalize_rel,
};
use rustc_hash::FxHashSet;

/// Where the fs plugin reports what the index left out. The engine routes a
/// report to the request whose id it names; `None` is a read with no request
/// (a BUILD evaluation's `heph.fs.glob`).
pub trait CodegenReport: Send + Sync {
    /// Put `kind` on the request's event stream (dropped with no request).
    fn emit(&self, request_id: Option<&str>, kind: BuildEventKind);
    /// Whether `key` is reported for the first time in this run: true once
    /// per key per request (per engine with no request).
    fn first_in_run(&self, request_id: Option<&str>, key: &str) -> bool;
}

/// Reports nothing and dedupes nothing: a driver built without an engine.
#[derive(Debug, Default)]
pub struct NoReport;

impl CodegenReport for NoReport {
    fn emit(&self, _request_id: Option<&str>, _kind: BuildEventKind) {}

    fn first_in_run(&self, _request_id: Option<&str>, _key: &str) -> bool {
        true
    }
}

/// The index and the reporter the fs driver and `heph.fs.glob` read through.
/// Without an index (a driver built for a unit test) every path is source.
#[derive(Clone)]
pub struct FsCodegen {
    index: Option<Arc<CodegenIndex>>,
    report: Arc<dyn CodegenReport>,
}

impl std::fmt::Debug for FsCodegen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsCodegen")
            .field("index", &self.index.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for FsCodegen {
    fn default() -> Self {
        Self {
            index: None,
            report: Arc::new(NoReport),
        }
    }
}

impl FsCodegen {
    pub fn new(index: Arc<CodegenIndex>, report: Arc<dyn CodegenReport>) -> Self {
        Self {
            index: Some(index),
            report,
        }
    }
}

/// The `status` field of a [`BuildEventKind::CodegenExcluded`].
pub fn status_str(kind: StatusKind) -> &'static str {
    match kind {
        StatusKind::Generated => "generated",
        StatusKind::Released => "released",
        StatusKind::Modified => "modified",
        StatusKind::Source => "source",
    }
}

/// The `reason` field of a [`BuildEventKind::CodegenExcluded`].
pub fn reason_str(reason: StatusReason) -> &'static str {
    match reason {
        StatusReason::Recorded => "recorded",
        StatusReason::Pending => "pending",
        StatusReason::Unrecorded => "unrecorded",
        StatusReason::Edited => "edited",
    }
}

/// One path a read left out, owned so it outlives the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub path: String,
    pub kind: StatusKind,
    pub reason: Option<StatusReason>,
    /// The target the deciding row names, if a row decided.
    pub addr: Option<String>,
}

impl Excluded {
    fn new(path: &str, st: &Status<'_>) -> Self {
        Self {
            path: path.to_owned(),
            kind: st.kind,
            reason: st.reason,
            addr: st.row.map(|r| r.addr.clone()),
        }
    }
}

/// The reasons a count is split by, in the order it names them.
const TALLY_REASONS: [StatusReason; 4] = [
    StatusReason::Recorded,
    StatusReason::Pending,
    StatusReason::Unrecorded,
    StatusReason::Edited,
];

/// How many paths a glob excluded, by reason: what it logs at `-v`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ExclusionTally {
    /// One slot per [`TALLY_REASONS`] entry, then one for no reason.
    counts: [usize; 5],
}

impl ExclusionTally {
    pub fn add(&mut self, reason: Option<StatusReason>) {
        let slot = reason
            .and_then(|r| TALLY_REASONS.iter().position(|&t| t == r))
            .unwrap_or(TALLY_REASONS.len());
        if let Some(c) = self.counts.get_mut(slot) {
            *c += 1;
        }
    }

    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }

    /// " (9 recorded, 2 unrecorded, 1 edited)", zero counts left out; empty
    /// when no count has a reason.
    fn breakdown(&self) -> String {
        let parts: Vec<String> = TALLY_REASONS
            .iter()
            .zip(self.counts)
            .filter(|(_, n)| *n > 0)
            .map(|(r, n)| format!("{n} {}", reason_str(*r)))
            .collect();
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        }
    }
}

/// "excluded 12 generated files (9 recorded, 2 unrecorded, 1 edited)": the
/// count a glob reports, split by reason, zero counts left out.
pub fn exclusion_summary(excluded: &[Excluded]) -> String {
    let mut tally = ExclusionTally::default();
    for e in excluded {
        tally.add(e.reason);
    }
    let n = tally.total();
    let noun = if n == 1 { "file" } else { "files" };
    format!("excluded {n} generated {noun}{}", tally.breakdown())
}

/// The warning for an fs `file` input of a path a section line covers and no
/// row records (E14): never "generated by", nothing recorded that.
pub fn unrecorded_message(path: &str, line_no: usize, pattern: &str, addr: &str) -> String {
    format!(
        "{path} matches section line `.gitignore:{line_no} {pattern}` (# {addr}) and is \
         excluded; depend on {addr}, or drop the line with heph tool codegen keep"
    )
}

/// Why an fs `file` target refused a generated path (Decision 18). The engine
/// turns [`Self::Generated`] into [`Self::TargetRemoved`] when the row's
/// target no longer resolves — the one def read, on the failure path only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodegenFileErrorKind {
    /// A recorded, pending or edited row whose target still produces it.
    Generated,
    /// A `dropped` row: its target no longer declares the output.
    NoLongerProduced,
    /// The row's target no longer exists.
    TargetRemoved,
}

/// An fs `file` target over a path heph generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodegenFileError {
    pub path: String,
    /// The generator the row names.
    pub addr: String,
    pub kind: CodegenFileErrorKind,
}

impl std::fmt::Display for CodegenFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { path, addr, kind } = self;
        let gone = match kind {
            CodegenFileErrorKind::Generated => {
                return write!(
                    f,
                    "{path} is generated by {addr}; depend on {addr}, not the file \
                     (heph tool codegen why {path})"
                );
            }
            CodegenFileErrorKind::NoLongerProduced => "no longer produces it",
            CodegenFileErrorKind::TargetRemoved => "no longer exists",
        };
        write!(
            f,
            "{path} was generated by {addr}, which {gone}; keep it with heph tool codegen keep \
             {path}, or delete it with heph tool codegen discard {path}"
        )
    }
}

impl std::error::Error for CodegenFileError {}

/// One glob walk's view of the index: one snapshot, territory carried down
/// the walk the way `dir_in_heph` is.
pub struct GlobLookup<'a> {
    index: &'a CodegenIndex,
    snap: Snapshot,
    root: &'a Path,
    /// The root-relative directory the walk stays under: all the snapshot
    /// loads (`CodegenIndex::snapshot_under`).
    scope: Vec<u8>,
}

impl<'a> GlobLookup<'a> {
    /// The snapshot of `walk_root` (the pattern's literal prefix, under
    /// `root`) this glob reads, or `None` with no index.
    pub fn open(
        cg: &'a FsCodegen,
        root: &'a Path,
        walk_root: &Path,
    ) -> anyhow::Result<Option<Self>> {
        let Some(index) = cg.index.as_deref() else {
            return Ok(None);
        };
        // Outside the root nothing is looked up; the whole index is right.
        let scope = normalize_rel(root, walk_root)
            .map(Cow::into_owned)
            .unwrap_or_default();
        let snap = index.snapshot_under(&scope).context_codegen()?;
        Ok(Some(Self {
            index,
            snap,
            root,
            scope,
        }))
    }

    /// Whether the walk's starting directory is territory: a point match,
    /// ancestors included.
    pub fn start_territory(&self, abs: &Path) -> bool {
        normalize_rel(self.root, abs)
            .is_some_and(|rel| self.snap.section().match_point(&rel, true).is_territory())
    }

    /// Whether directory `abs` is territory, given its parent's answer.
    pub fn dir_territory(&self, abs: &Path, parent_territory: bool) -> bool {
        parent_territory
            || normalize_rel(self.root, abs)
                .is_some_and(|rel| self.snap.section().dir_is_territory(&rel, false))
    }

    /// The exclusion of the file at root-relative `rel`, walked under a
    /// directory whose territory is `parent_territory` (`None`: a lone path,
    /// tested as a point lookup). `None` when the glob keeps it.
    ///
    /// Takes the caller's root-relative path rather than the absolute one: a
    /// second component-wise `strip_prefix` per file was ~5% of a glob walk.
    pub fn exclusion(
        &self,
        rel: &str,
        parent_territory: Option<bool>,
    ) -> anyhow::Result<Option<Excluded>> {
        let Some(nrel) = normalize_rel(self.root, Path::new(rel)) else {
            return Ok(None);
        };
        // A glob keeps or drops; an edited generated file is dropped like an
        // intact one, so the lookup skips its lstat (`why` tells them apart).
        let meta = match parent_territory {
            Some(p) => LookupMeta::walk(false, p),
            None => LookupMeta::point(false),
        }
        .exclusion_only();
        let st = self.snap.status(&nrel, &meta).with_context_path(rel)?;
        Ok(st.kind.is_excluded().then(|| Excluded::new(rel, &st)))
    }

    /// End of walk: a commit since the snapshot can turn a kept path into an
    /// excluded one (a new pending or written row). Reclassify the kept
    /// artifacts those rows name; only a kept → excluded move acts, an
    /// exclusion stays for this glob.
    pub fn recheck(
        &self,
        artifacts: &mut Vec<OutputArtifact>,
        excluded: &mut Vec<Excluded>,
    ) -> anyhow::Result<()> {
        let rows = self.index.end_of_walk(&self.snap).context_codegen()?;
        if rows.is_empty() {
            return Ok(());
        }
        let moved: FxHashSet<&[u8]> = rows.iter().map(|r| r.path.as_slice()).collect();
        let named = |a: &OutputArtifact| match &a.content {
            Content::File(f) => moved.contains(f.out_path.as_bytes()),
            Content::Raw(_) | Content::TarPath(_) | Content::CpioPath(_) | Content::View(_) => {
                false
            }
        };
        if !artifacts.iter().any(named) {
            return Ok(());
        }
        let fresh = self.index.snapshot_under(&self.scope).context_codegen()?;
        let mut err = None;
        artifacts.retain(|a| {
            if err.is_some() || !named(a) {
                return true;
            }
            let Content::File(f) = &a.content else {
                return true;
            };
            match fresh.status_path(Path::new(&f.out_path), false) {
                Ok(st) if st.kind.is_excluded() => {
                    excluded.push(Excluded::new(&f.out_path, &st));
                    false
                }
                Ok(_) => true,
                Err(e) => {
                    err = Some(e);
                    true
                }
            }
        });
        match err {
            Some(e) => Err(e).context_codegen(),
            None => Ok(()),
        }
    }
}

/// What an fs `file` or `copy_output` target does with its one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// `file`: a generated path is an error, an unrecorded match warns.
    Strict,
    /// `copy_output`: an excluded path is zero artifacts, silently.
    CopyOutput,
}

/// What the fs target over a path does with it.
#[derive(Debug, PartialEq, Eq)]
pub enum FileSource {
    /// Reads the file.
    Read,
    /// Zero artifacts. `warning` is the unrecorded-line warning, set the
    /// first time in a run: the caller logs it ([`FileWarning::log`]) from
    /// its own task, not from the blocking job this runs on, so it carries
    /// the run's span and subscriber.
    Excluded { warning: Option<FileWarning> },
}

/// The warning for an fs `file` input of an unrecorded section match (E14).
#[derive(Debug, PartialEq, Eq)]
pub struct FileWarning {
    path: String,
    line_no: usize,
    message: String,
}

impl FileWarning {
    pub fn log(&self) {
        tracing::warn!(path = self.path, line = self.line_no, "{}", self.message);
    }
}

/// Whether the fs target over `path` (root-relative, normalised) reads it.
/// An error is a [`CodegenFileError`].
pub fn file_is_source(
    cg: &FsCodegen,
    root: &Path,
    path: &str,
    mode: FileMode,
    request_id: Option<&str>,
) -> anyhow::Result<FileSource> {
    let Some(index) = cg.index.as_deref() else {
        return Ok(FileSource::Read);
    };
    let Some(rel) = normalize_rel(root, Path::new(path)) else {
        return Ok(FileSource::Read);
    };
    // Only the file's directory is loaded.
    let dir = rel
        .iter()
        .rposition(|&b| b == b'/')
        .and_then(|i| rel.get(..i))
        .unwrap_or_default();
    let snap = index.snapshot_under(dir).context_codegen()?;
    let st = snap
        .status(&rel, &LookupMeta::point(false))
        .with_context_path(path)?;
    if !st.kind.is_excluded() {
        return Ok(FileSource::Read);
    }
    if mode == FileMode::CopyOutput {
        return Ok(FileSource::Excluded { warning: None });
    }
    if let Some(row) = st.row {
        return Err(CodegenFileError {
            path: path.to_owned(),
            addr: row.addr.clone(),
            kind: if row.state == RowState::Dropped {
                CodegenFileErrorKind::NoLongerProduced
            } else {
                CodegenFileErrorKind::Generated
            },
        }
        .into());
    }
    // Unrecorded under a line (E14): zero artifacts; a warning names the line
    // when heph wrote it. A foreign line is `status`'s to list, not a build's.
    let ex = Excluded::new(path, &st);
    report(cg, request_id, std::slice::from_ref(&ex));
    let line = st.line().and_then(|i| snap.section().line(i));
    let warning = line
        .filter(|l| !l.is_foreign())
        .and_then(|line| line.entry.addr.as_ref().map(|addr| (line, addr)))
        .filter(|_| {
            cg.report
                .first_in_run(request_id, &format!("unrecorded\0{path}"))
        })
        .map(|(line, addr)| FileWarning {
            path: path.to_owned(),
            line_no: line.line_no,
            message: unrecorded_message(path, line.line_no, &line.entry.pattern, &addr.format()),
        });
    Ok(FileSource::Excluded { warning })
}

/// Report what a read excluded: one `CodegenExcluded` per path per run (the
/// path at debug).
pub fn report(cg: &FsCodegen, request_id: Option<&str>, excluded: &[Excluded]) {
    for ex in excluded {
        let path = ex.path.as_str();
        if cg
            .report
            .first_in_run(request_id, &format!("excluded\0{path}"))
        {
            let status = status_str(ex.kind);
            let reason = ex.reason.map(reason_str);
            tracing::debug!(
                path,
                status,
                reason,
                addr = ex.addr.as_deref(),
                "codegen: excluded"
            );
            cg.report.emit(
                request_id,
                BuildEventKind::CodegenExcluded {
                    path: path.to_owned(),
                    status: status.to_owned(),
                    reason: reason.map(str::to_owned),
                    addr: ex.addr.clone(),
                },
            );
        }
    }
}

/// Context for an index failure, naming the index rather than the path.
trait CodegenContext<T> {
    fn context_codegen(self) -> anyhow::Result<T>;
    fn with_context_path(self, path: &str) -> anyhow::Result<T>;
}

impl<T> CodegenContext<T> for anyhow::Result<T> {
    fn context_codegen(self) -> anyhow::Result<T> {
        anyhow::Context::context(self, "reading the codegen index")
    }

    fn with_context_path(self, path: &str) -> anyhow::Result<T> {
        anyhow::Context::with_context(self, || format!("looking up {path} in the codegen index"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(reason: StatusReason) -> Excluded {
        Excluded {
            path: "p/a.go".to_owned(),
            kind: if reason == StatusReason::Edited {
                StatusKind::Modified
            } else {
                StatusKind::Generated
            },
            reason: Some(reason),
            addr: None,
        }
    }

    #[test]
    fn summary_names_reasons_in_order_and_omits_zeroes() {
        let mut v: Vec<Excluded> = std::iter::repeat_with(|| ex(StatusReason::Recorded))
            .take(9)
            .collect();
        v.push(ex(StatusReason::Unrecorded));
        v.push(ex(StatusReason::Unrecorded));
        v.push(ex(StatusReason::Edited));
        assert_eq!(
            exclusion_summary(&v),
            "excluded 12 generated files (9 recorded, 2 unrecorded, 1 edited)"
        );
        assert_eq!(
            exclusion_summary(&[ex(StatusReason::Pending)]),
            "excluded 1 generated file (1 pending)"
        );
    }

    #[test]
    fn file_error_wording() {
        let e = |kind| {
            CodegenFileError {
                path: "p/foo.pb.go".to_owned(),
                addr: "//p:gen".to_owned(),
                kind,
            }
            .to_string()
        };
        assert_eq!(
            e(CodegenFileErrorKind::Generated),
            "p/foo.pb.go is generated by //p:gen; depend on //p:gen, not the file \
             (heph tool codegen why p/foo.pb.go)"
        );
        assert_eq!(
            e(CodegenFileErrorKind::NoLongerProduced),
            "p/foo.pb.go was generated by //p:gen, which no longer produces it; keep it with \
             heph tool codegen keep p/foo.pb.go, or delete it with heph tool codegen discard \
             p/foo.pb.go"
        );
        assert!(e(CodegenFileErrorKind::TargetRemoved).contains("which no longer exists; keep"));
    }

    #[test]
    fn unrecorded_warning_names_the_line_never_generated_by() {
        let m = unrecorded_message("p/foo.pb.go", 42, "/p/*.pb.go", "//p:gen");
        assert_eq!(
            m,
            "p/foo.pb.go matches section line `.gitignore:42 /p/*.pb.go` (# //p:gen) and is \
             excluded; depend on //p:gen, or drop the line with heph tool codegen keep"
        );
        assert!(!m.contains("generated by"));
    }
}
