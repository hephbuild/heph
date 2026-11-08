//! `heph tool codegen why <path>...`: why a path is, or is not, read as
//! generated. Works on any path, `Source` included; reads only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use serde_json::json;

use super::keep::PathResultKind;
use super::{
    JSON_SCHEMA, Report, ResolvedPath, blocking, display, empty_index_hint, resolve_path,
    root_hint, run_cli, with_hint,
};
use crate::commands::GlobalOptions;
use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use crate::tui::LogSink;
use hwalk::codegen_index::fs::lstat_opt;
use hwalk::codegen_index::paths::rel_path;
use hwalk::codegen_index::{LookupMeta, Row, RowState, StatusKind, StatusReason};

#[derive(clap::Args, Clone, Debug, Default)]
pub struct Args {
    /// Paths to explain (cwd-relative, //root-relative or absolute)
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<String>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

/// The row that decided, as `why` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyRow {
    /// The row's own path: another casing of the argument after a fold hit.
    pub path: String,
    pub addr: String,
    pub output: Option<String>,
    /// `written`, `imported` or `dropped`.
    pub state: &'static str,
    /// ns since the epoch, when the record was taken.
    pub written_at: Option<i64>,
    pub pending: bool,
    /// How the bytes were checked: `stat`, `hash`, `another_casing` (same
    /// inode under the row's path), `pending` or `missing`.
    pub verified: &'static str,
}

/// The section line that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyLine {
    pub line: usize,
    pub pattern: String,
    /// Attributed to a target and root-anchored: heph wrote it.
    pub heph_wrote: bool,
    /// `true` when the line re-includes the path (`!pattern`).
    pub negated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyItem {
    pub path: String,
    pub result: PathResultKind,
    pub reason: Option<String>,
    /// `generated`, `released`, `modified` or `source`.
    pub status: Option<&'static str>,
    /// `recorded`, `pending`, `unrecorded` or `edited`.
    pub status_reason: Option<&'static str>,
    pub row: Option<WhyRow>,
    pub section_line: Option<WhyLine>,
}

#[derive(Debug, Clone, Default)]
pub struct WhyReport {
    /// In argument order, deduplicated.
    pub items: Vec<WhyItem>,
    pub hint: Option<String>,
}

fn kind_str(k: StatusKind) -> &'static str {
    match k {
        StatusKind::Generated => "generated",
        StatusKind::Released => "released",
        StatusKind::Modified => "modified",
        StatusKind::Source => "source",
    }
}

fn reason_str(r: StatusReason) -> &'static str {
    match r {
        StatusReason::Recorded => "recorded",
        StatusReason::Pending => "pending",
        StatusReason::Unrecorded => "unrecorded",
        StatusReason::Edited => "edited",
    }
}

fn state_str(s: RowState) -> &'static str {
    match s {
        RowState::Written => "written",
        RowState::Imported => "imported",
        RowState::Dropped => "dropped",
    }
}

impl Report for WhyReport {
    fn text(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in &self.items {
            let Some(status) = i.status else {
                out.push(format!(
                    "{}: {} ({})",
                    i.path,
                    i.result.as_str(),
                    i.reason.as_deref().unwrap_or("")
                ));
                continue;
            };
            match i.status_reason {
                Some(r) => out.push(format!("{}: {status} ({r})", i.path)),
                None => out.push(format!("{}: {status}", i.path)),
            }
            match &i.row {
                Some(row) => {
                    out.push(format!(
                        "  row: {} output {} state {}{}",
                        row.addr,
                        row.output.as_deref().unwrap_or("(none)"),
                        row.state,
                        if row.pending { ", pending" } else { "" }
                    ));
                    if row.path != i.path {
                        out.push(format!("  recorded as {} (same file)", row.path));
                    }
                    out.push(format!("  verified by {}", row.verified));
                    if let Some(w) = row.written_at {
                        out.push(format!("  written_at {w}"));
                    }
                }
                None => out.push("  no row records it".to_string()),
            }
            match &i.section_line {
                Some(l) => out.push(format!(
                    "  .gitignore:{} {}{}",
                    l.line,
                    l.pattern,
                    match (l.negated, l.heph_wrote) {
                        (true, _) => " (re-includes it)",
                        (false, true) => " (written by heph)",
                        (false, false) => " (not written by heph)",
                    }
                )),
                None => out.push("  no section line matches".to_string()),
            }
            if let Some(why) = &i.reason {
                out.push(format!("  {why}"));
            }
        }
        if let Some(h) = &self.hint {
            out.push(h.clone());
        }
        out
    }

    fn json(&self) -> serde_json::Value {
        let results: Vec<serde_json::Value> = self
            .items
            .iter()
            .map(|i| {
                json!({
                    "path": i.path,
                    "result": i.result.as_str(),
                    "reason": i.reason,
                    "status": i.status,
                    "status_reason": i.status_reason,
                    "row": i.row.as_ref().map(|r| json!({
                        "path": r.path,
                        "addr": r.addr,
                        "output": r.output,
                        "state": r.state,
                        "written_at": r.written_at,
                        "pending": r.pending,
                        "verified": r.verified,
                    })),
                    "section_line": i.section_line.as_ref().map(|l| json!({
                        "line": l.line,
                        "pattern": l.pattern,
                        "heph_wrote": l.heph_wrote,
                        "negated": l.negated,
                    })),
                })
            })
            .collect();
        let mut doc = json!({ "schema": JSON_SCHEMA, "results": results });
        if let (Some(h), Some(m)) = (&self.hint, doc.as_object_mut()) {
            m.insert("hint".into(), json!(h));
        }
        doc
    }

    fn exit_code(&self) -> u8 {
        u8::from(self.items.iter().any(|i| i.result.is_failure()))
    }
}

/// How `row`'s bytes check out at `abs`.
fn verified_by(
    index: &hwalk::codegen_index::CodegenIndex,
    row: &Row,
    asked: &[u8],
) -> anyhow::Result<&'static str> {
    if row.pending_hash.is_some() {
        return Ok("pending");
    }
    if row.path != asked {
        return Ok("another_casing");
    }
    let abs = index.root().join(rel_path(&row.path));
    let Some(st) = lstat_opt(index.fs().as_ref(), &abs)? else {
        return Ok("missing");
    };
    let stat_equal = row.record.as_ref().is_some_and(|r| {
        r.size == st.size
            && r.mtime_ns == st.mtime_ns
            && r.ino == st.ino
            && r.dev == st.dev
            && r.mtime_ns < r.written_at_ns
    });
    Ok(if stat_equal { "stat" } else { "hash" })
}

/// Explain each path. Reads only: no lock, no write (J6).
pub async fn why(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    paths: &[String],
    cwd: &Path,
) -> anyhow::Result<WhyReport> {
    engine.open_codegen_index(rs).await?;
    let index = Arc::clone(engine.codegen_index());
    let root = engine.root().to_path_buf();
    let (paths, cwd) = (paths.to_vec(), cwd.to_path_buf());
    let items = blocking(move || {
        let snap = index.snapshot()?;
        let section = snap.section();
        let mut seen = std::collections::BTreeSet::new();
        let mut items = Vec::new();
        for arg in paths {
            let rel = match resolve_path(&root, &cwd, &arg) {
                ResolvedPath::Inside(rel) => rel,
                ResolvedPath::Outside => {
                    items.push(WhyItem {
                        path: arg,
                        result: PathResultKind::Refused,
                        reason: Some("outside the workspace".into()),
                        status: None,
                        status_reason: None,
                        row: None,
                        section_line: None,
                    });
                    continue;
                }
            };
            if !seen.insert(rel.clone()) {
                continue;
            }
            let abs = root.join(rel_path(&rel));
            let st = lstat_opt(index.fs().as_ref(), &abs)
                .with_context(|| format!("reading {}", abs.display()))?;
            let is_dir = st
                .as_ref()
                .is_some_and(|s| s.kind == hwalk::codegen_index::StatKind::Dir);
            let status = snap.status(&rel, &LookupMeta::point(is_dir))?;
            // A walk-shaped answer carries no line; ask the point match.
            let matched = match status.line() {
                Some(_) => status.matched,
                None => match status.row {
                    Some(r) => section.match_point(&r.path, is_dir),
                    None => section.match_point(&rel, is_dir),
                },
            };
            let section_line = matched
                .line()
                .and_then(|i| section.line(i))
                .map(|l| WhyLine {
                    line: l.line_no,
                    pattern: l.entry.pattern.clone(),
                    heph_wrote: !l.is_foreign(),
                    negated: matches!(matched, hwalk::codegen_index::SectionMatch::Negated(_)),
                });
            let row = match status.row {
                Some(r) => Some(WhyRow {
                    path: display(&r.path),
                    addr: r.addr.clone(),
                    output: r.output.clone(),
                    state: state_str(r.state),
                    written_at: r.record.as_ref().map(|rec| rec.written_at_ns),
                    pending: r.pending_hash.is_some(),
                    verified: verified_by(&index, r, &rel)?,
                }),
                None => None,
            };
            let unknown = st.is_none() && row.is_none() && section_line.is_none();
            let reason = if unknown {
                Some(with_hint(
                    "no such file, no row and no section line",
                    root_hint(&root, &cwd, &arg).as_deref(),
                ))
            } else {
                match (status.kind, row.as_ref()) {
                    (StatusKind::Modified, Some(r)) => Some(format!(
                        "edited since {} wrote it; the next build of it overwrites it",
                        r.addr
                    )),
                    (k, Some(r)) if k == StatusKind::Released || r.state == "dropped" => Some(
                        format!("no longer generated (was {}); keep or discard it", r.addr),
                    ),
                    _ => None,
                }
            };
            items.push(WhyItem {
                path: display(&rel),
                result: if unknown {
                    PathResultKind::Unknown
                } else {
                    PathResultKind::Explained
                },
                reason,
                status: (!unknown).then(|| kind_str(status.kind)),
                status_reason: status.reason.map(reason_str),
                row,
                section_line,
            });
        }
        Ok(items)
    })
    .await?;
    // The hint only when a path matched a section line (spec: Upgrade).
    let hint = if items.iter().any(|i: &WhyItem| i.section_line.is_some()) {
        empty_index_hint(engine).await?
    } else {
        None
    };
    Ok(WhyReport { items, hint })
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    let paths = args.paths.clone();
    let cwd: PathBuf = std::env::current_dir().context("reading the working directory")?;
    run_cli(
        "Explaining",
        args.json,
        sink,
        global,
        move |engine, rs| async move { why(&engine, &rs, &paths, &cwd).await },
    )
}
