//! `heph tool codegen keep <path>...` and `discard <path>... | --all`: make
//! generated files source, or delete the ones heph wrote and no target
//! produces any more. Both drop the rows and the section lines a full
//! `codegen gitignore` would drop for those paths (J8).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use serde_json::json;

use super::status::{AddrState, resolve_addrs};
use super::{
    JSON_SCHEMA, Report, ResolvedPath, blocking, display, read_gitignore, resolve_path, root_hint,
    run_cli, with_hint,
};
use crate::commands::GlobalOptions;
use crate::engine::gitignore::{self, CopyTargets, GitignoreEntry};
use crate::engine::request_state::RequestState;
use crate::engine::{CopyOutputs, Discovery, Engine};
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::tui::LogSink;
use hwalk::codegen_index::commands::DiscardOutcome;
use hwalk::codegen_index::{LookupMeta, RowState, Section, StatusKind};

#[derive(clap::Args, Clone, Debug, Default)]
pub struct KeepArgs {
    /// Files to make source (cwd-relative, //root-relative or absolute)
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<String>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone, Debug, Default)]
pub struct DiscardArgs {
    /// Files to delete (cwd-relative, //root-relative or absolute)
    #[arg(
        value_name = "PATH",
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    pub paths: Vec<String>,
    /// Every file `status` lists as no longer generated, and every dangling row
    #[arg(long)]
    pub all: bool,
    /// Print what would be deleted and dropped; change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Delete an edited or imported file too
    #[arg(long)]
    pub force: bool,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

/// One path's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathResultKind {
    Kept,
    Discarded,
    WouldDiscard,
    Refused,
    Unknown,
    /// `why`: the path is explained.
    Explained,
}

impl PathResultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explained => "explained",
            Self::Kept => "kept",
            Self::Discarded => "discarded",
            Self::WouldDiscard => "would_discard",
            Self::Refused => "refused",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Self::Refused | Self::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathResult {
    /// As given when refused for being outside the workspace; root-relative
    /// otherwise.
    pub path: String,
    pub result: PathResultKind,
    pub reason: Option<String>,
}

/// What `keep` or `discard` did.
#[derive(Debug, Clone, Default)]
pub struct PathsReport {
    /// Sorted by path.
    pub results: Vec<PathResult>,
    /// The section lines dropped (or, dry run, that would be), sorted.
    pub lines_removed: Vec<GitignoreEntry>,
    /// Other recorded files the dropped lines covered, now released; sorted.
    pub also_released: Vec<String>,
    pub dry_run: bool,
    /// `keep`: print the `git add` to commit the kept files.
    pub git_add: bool,
}

impl Report for PathsReport {
    fn text(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.results {
            match &r.reason {
                Some(why) => out.push(format!("{} {}: {why}", r.result.as_str(), r.path)),
                None => out.push(format!("{} {}", r.result.as_str(), r.path)),
            }
        }
        let verb = if self.dry_run {
            "would drop"
        } else {
            "dropped"
        };
        for l in &self.lines_removed {
            match &l.addr {
                Some(a) => out.push(format!(
                    "{verb} .gitignore line {} ({})",
                    l.pattern,
                    a.format()
                )),
                None => out.push(format!("{verb} .gitignore line {}", l.pattern)),
            }
        }
        for p in &self.also_released {
            out.push(format!(
                "released {p}: a dropped line covered it; now untracked; keep or discard it"
            ));
        }
        if self.git_add {
            let kept: Vec<&str> = self
                .results
                .iter()
                .filter(|r| r.result == PathResultKind::Kept)
                .map(|r| r.path.as_str())
                .collect();
            if !kept.is_empty() {
                out.push(format!("git add {} .gitignore", kept.join(" ")));
            }
        }
        out
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "schema": JSON_SCHEMA,
            "results": self.results.iter().map(|r| json!({
                "path": r.path,
                "result": r.result.as_str(),
                "reason": r.reason,
            })).collect::<Vec<_>>(),
            "lines": self.lines_removed.iter().map(|l| json!({
                "op": "remove",
                "pattern": l.pattern,
                "addr": l.addr.as_ref().map(crate::htaddr::Addr::format),
            })).collect::<Vec<_>>(),
            "released": self.also_released,
        })
    }

    fn exit_code(&self) -> u8 {
        u8::from(self.results.iter().any(|r| r.result.is_failure()))
    }
}

/// Paths named on the command line.
#[derive(Debug, Clone)]
pub enum Selection {
    Paths(Vec<String>),
    /// `discard --all`.
    All,
}

/// `keep` / `discard` options. `cwd` resolves relative paths.
#[derive(Debug, Clone)]
pub struct Options {
    pub selection: Selection,
    pub cwd: PathBuf,
    pub dry_run: bool,
    pub force: bool,
}

/// Every current target's copy outputs, with where it is declared.
struct Declared {
    targets: Vec<(String, String, CopyOutputs)>,
}

impl Declared {
    fn of(copy: &CopyTargets) -> anyhow::Result<Self> {
        let mut targets = Vec::with_capacity(copy.targets.len());
        for (addr, def) in &copy.targets {
            let pkg = addr.package.to_string();
            let build = if pkg.is_empty() {
                "BUILD".to_string()
            } else {
                format!("{pkg}/BUILD")
            };
            targets.push((addr.format(), build, CopyOutputs::of(def)?));
        }
        Ok(Self { targets })
    }

    /// The (addr, output, BUILD file) declaring `rel` as a copy output.
    fn declaring(&self, rel: &[u8]) -> Option<(&str, &str, &str)> {
        self.targets.iter().find_map(|(addr, build, outs)| {
            outs.output_of(rel)
                .map(|g| (addr.as_str(), g, build.as_str()))
        })
    }
}

/// An output group as a message names it: the unnamed group of `out = "x"`
/// is the target's default output, not an empty name.
fn output_name(group: &str) -> String {
    if group.is_empty() {
        "its default output".to_string()
    } else {
        format!("output `{group}`")
    }
}

/// The `reason` of an `unknown` result.
const NOT_GENERATED: &str = "not generated: no row records it and no section line covers it";

fn result(path: &str, result: PathResultKind, reason: Option<String>) -> PathResult {
    PathResult {
        path: path.to_string(),
        result,
        reason,
    }
}

/// Resolve the named paths, each with its [`root_hint`] for an `unknown`
/// result; the ones outside the workspace are refused.
fn resolve_named(
    root: &Path,
    cwd: &Path,
    args: &[String],
    results: &mut Vec<PathResult>,
) -> Vec<(Vec<u8>, Option<String>)> {
    let mut inside = BTreeMap::new();
    for a in args {
        match resolve_path(root, cwd, a) {
            ResolvedPath::Inside(rel) => {
                inside.entry(rel).or_insert_with(|| root_hint(root, cwd, a));
            }
            ResolvedPath::Outside => results.push(result(
                a,
                PathResultKind::Refused,
                Some("outside the workspace".into()),
            )),
        }
    }
    inside.into_iter().collect()
}

/// Under the section lock: drop the section lines covering any of `paths`
/// that a full `codegen gitignore` would drop (they are not in `fresh`), and
/// list the other recorded files those lines covered. `dry_run` writes
/// nothing.
async fn drop_covering_lines(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    paths: Vec<Vec<u8>>,
    fresh: Vec<GitignoreEntry>,
    dry_run: bool,
) -> anyhow::Result<(Vec<GitignoreEntry>, Vec<String>)> {
    if paths.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let guard = engine.codegen_locks().section(rs.ctoken()).await?;
    let index = Arc::clone(engine.codegen_index());
    let root = engine.root().to_path_buf();
    blocking(move || {
        let _held = guard;
        let existing = read_gitignore(&root)?;
        let parsed = hwalk::gitignore_section::parse_section_bytes(&existing);
        let keep_key: BTreeSet<(String, String)> = fresh
            .iter()
            .map(|e| {
                let (p, a) = e.sort_key();
                (p.to_string(), a)
            })
            .collect();
        let mut remaining = Vec::new();
        let mut dropped: Vec<(GitignoreEntry, Section)> = Vec::new();
        for line in parsed.lines {
            let (p, a) = line.entry.sort_key();
            if line.is_comment() || keep_key.contains(&(p.to_string(), a)) {
                remaining.push(line.entry);
                continue;
            }
            let one = Section::compile(&gitignore::render(b"", std::slice::from_ref(&line.entry)));
            if paths
                .iter()
                .any(|path| one.match_point(path, false).is_territory())
            {
                dropped.push((line.entry, one));
            } else {
                remaining.push(line.entry);
            }
        }
        if dropped.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        if !dry_run {
            let path = root.join(".gitignore");
            std::fs::write(&path, gitignore::render(&existing, &remaining))
                .with_context(|| format!("writing {}", path.display()))?;
        }
        let snap = index.snapshot()?;
        let named: BTreeSet<&[u8]> = paths.iter().map(Vec::as_slice).collect();
        let also: BTreeSet<String> = snap
            .rows()
            .filter(|r| !named.contains(r.path.as_slice()))
            .filter(|r| {
                dropped
                    .iter()
                    .any(|(_, s)| s.match_point(&r.path, false).is_territory())
            })
            .map(|r| display(&r.path))
            .collect();
        let mut lines: Vec<GitignoreEntry> = dropped.into_iter().map(|(e, _)| e).collect();
        lines.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        Ok((lines, also.into_iter().collect()))
    })
    .await
}

/// The defs `keep` and `discard` need, resolved before any lock (J9).
async fn current_targets(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
) -> anyhow::Result<(CopyTargets, Declared)> {
    let copy = Arc::clone(engine)
        .codegen_copy_targets(
            rs.clone(),
            &Matcher::TreeOutputTo(PkgBuf::from("")),
            Discovery::Complete,
            false,
        )
        .await?;
    let declared = Declared::of(&copy)?;
    Ok((copy, declared))
}

/// `codegen keep`: make each path source from now on, recorded or not. A
/// path a current target declares as a `copy` output is refused, naming the
/// output and its BUILD file. Otherwise its xattr is stripped (Decision 24)
/// and its row dropped, each target under its own lock, then the section
/// lines that covered it are dropped under the section lock.
pub async fn keep(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    paths: &[String],
    cwd: &Path,
) -> anyhow::Result<PathsReport> {
    engine.open_codegen_index(rs).await?;
    let root = engine.root().to_path_buf();
    let mut results = Vec::new();
    let named = resolve_named(&root, cwd, paths, &mut results);
    let (copy, declared) = current_targets(engine, rs).await?;

    let index = Arc::clone(engine.codegen_index());
    let (by_addr, unrecorded, mut decided) = blocking({
        let index = Arc::clone(&index);
        move || {
            let snap = index.snapshot()?;
            let mut by_addr: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
            let mut unrecorded = Vec::new();
            let mut decided = Vec::new();
            for (rel, hint) in named {
                let shown = display(&rel);
                if let Some((addr, output, build)) = declared.declaring(&rel) {
                    decided.push(result(
                        &shown,
                        PathResultKind::Refused,
                        Some(format!(
                            "{addr} declares it as a copy output ({} in {build}); remove the output first",
                            output_name(output)
                        )),
                    ));
                    continue;
                }
                if let Some(row) = snap.row(&rel) {
                    by_addr.entry(row.addr.clone()).or_default().push(rel);
                } else if snap.section().match_point(&rel, false).is_territory() {
                    unrecorded.push(rel);
                } else {
                    decided.push(result(
                        &shown,
                        PathResultKind::Unknown,
                        Some(with_hint(NOT_GENERATED, hint.as_deref())),
                    ));
                }
            }
            Ok((by_addr, unrecorded, decided))
        }
    })
    .await?;
    results.append(&mut decided);

    let mut kept: Vec<Vec<u8>> = Vec::new();
    for (addr, paths) in by_addr {
        let guard = engine.codegen_locks().target(&addr, rs.ctoken()).await?;
        let index = Arc::clone(&index);
        let root = root.clone();
        let dropped = blocking(move || {
            for p in &paths {
                hwalk::codegen_index::import::strip_xattr(
                    &root.join(hwalk::codegen_index::paths::rel_path(p)),
                )?;
            }
            index.forget(&guard, &paths)?;
            Ok(paths)
        })
        .await?;
        kept.extend(dropped);
    }
    let unrecorded = blocking({
        let root = root.clone();
        move || {
            for p in &unrecorded {
                hwalk::codegen_index::import::strip_xattr(
                    &root.join(hwalk::codegen_index::paths::rel_path(p)),
                )?;
            }
            Ok(unrecorded)
        }
    })
    .await?;
    kept.extend(unrecorded);
    kept.sort();
    for p in &kept {
        results.push(result(&display(p), PathResultKind::Kept, None));
    }

    let (lines_removed, also_released) =
        drop_covering_lines(engine, rs, kept, copy.entries(), false).await?;
    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(PathsReport {
        results,
        lines_removed,
        also_released,
        dry_run: false,
        git_add: true,
    })
}

/// `codegen discard`: delete files heph wrote that no target produces any
/// more, after a fresh hash equals the record (J5); `force` lifts that for an
/// edited or imported file. A path a current target declares is refused
/// (rebuild instead), and so is a file heph never wrote. `--all` covers every
/// file no longer generated and every dangling row.
pub async fn discard(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    opts: &Options,
) -> anyhow::Result<PathsReport> {
    engine.open_codegen_index(rs).await?;
    let root = engine.root().to_path_buf();
    let mut results = Vec::new();
    let named = match &opts.selection {
        Selection::Paths(p) => Some(resolve_named(&root, &opts.cwd, p, &mut results)),
        Selection::All => None,
    };
    let (copy, declared) = current_targets(engine, rs).await?;
    let index = Arc::clone(engine.codegen_index());

    // `--all` needs every row's target, to tell the ones no longer generated.
    let states = if named.is_none() {
        let addrs = blocking({
            let index = Arc::clone(&index);
            move || {
                let snap = index.snapshot()?;
                Ok(snap.rows().map(|r| r.addr.clone()).collect::<BTreeSet<_>>())
            }
        })
        .await?;
        resolve_addrs(engine, rs, addrs, &copy).await?
    } else {
        BTreeMap::new()
    };

    let (by_addr, mut decided) = blocking({
        let index = Arc::clone(&index);
        move || {
            let snap = index.snapshot()?;
            let mut by_addr: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
            let mut decided = Vec::new();
            match named {
                Some(named) => {
                    for (rel, hint) in named {
                        let shown = display(&rel);
                        if let Some((addr, output, _)) = declared.declaring(&rel) {
                            decided.push(result(
                                &shown,
                                PathResultKind::Refused,
                                Some(format!(
                                    "{addr} still declares it ({}); rebuild {addr} instead",
                                    output_name(output)
                                )),
                            ));
                        } else if let Some(row) = snap.row(&rel) {
                            by_addr.entry(row.addr.clone()).or_default().push(rel);
                        } else if snap.section().match_point(&rel, false).is_territory() {
                            decided.push(result(
                                &shown,
                                PathResultKind::Refused,
                                Some("heph never wrote it; keep it, or delete it yourself".into()),
                            ));
                        } else {
                            decided.push(result(
                                &shown,
                                PathResultKind::Unknown,
                                Some(with_hint(NOT_GENERATED, hint.as_deref())),
                            ));
                        }
                    }
                }
                None => {
                    for row in snap.rows() {
                        if row.pending_hash.is_some() {
                            continue;
                        }
                        let abs = root.join(hwalk::codegen_index::paths::rel_path(&row.path));
                        let gone = hwalk::codegen_index::fs::lstat_opt(index.fs().as_ref(), &abs)?
                            .is_none();
                        let live_declared = declared.declaring(&row.path).is_some();
                        let no_longer = row.state == RowState::Dropped
                            || match states.get(&row.addr) {
                                Some(AddrState::Live(outs)) => !outs.contains(&row.path),
                                Some(AddrState::Missing) => true,
                                Some(AddrState::Unresolved { .. }) | None => false,
                            }
                            || snap.status(&row.path, &LookupMeta::point(false))?.kind
                                == StatusKind::Released;
                        if gone || (no_longer && !live_declared) {
                            by_addr
                                .entry(row.addr.clone())
                                .or_default()
                                .push(row.path.clone());
                        }
                    }
                }
            }
            Ok((by_addr, decided))
        }
    })
    .await?;
    results.append(&mut decided);

    let mut affected = Vec::new();
    for (addr, paths) in by_addr {
        let guard = engine.codegen_locks().target(&addr, rs.ctoken()).await?;
        let index = Arc::clone(&index);
        let (force, dry_run) = (opts.force, opts.dry_run);
        let outcomes = blocking(move || index.discard(&guard, &paths, force, dry_run)).await?;
        for d in outcomes {
            let shown = display(&d.path);
            let r = match d.outcome {
                DiscardOutcome::Discarded => result(&shown, PathResultKind::Discarded, None),
                DiscardOutcome::Gone => result(
                    &shown,
                    PathResultKind::Discarded,
                    Some("already gone; its row is dropped".into()),
                ),
                DiscardOutcome::WouldDiscard { gone } => result(
                    &shown,
                    PathResultKind::WouldDiscard,
                    gone.then(|| "already gone; its row would be dropped".into()),
                ),
                DiscardOutcome::Refused(why) => result(&shown, PathResultKind::Refused, Some(why)),
            };
            if !r.result.is_failure() {
                affected.push(d.path);
            }
            results.push(r);
        }
    }
    affected.sort();

    let (lines_removed, also_released) =
        drop_covering_lines(engine, rs, affected, copy.entries(), opts.dry_run).await?;
    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(PathsReport {
        results,
        lines_removed,
        also_released,
        dry_run: opts.dry_run,
        git_add: false,
    })
}

fn cwd() -> anyhow::Result<PathBuf> {
    std::env::current_dir().context("reading the working directory")
}

pub fn execute_keep(args: &KeepArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    let paths = args.paths.clone();
    let cwd = cwd()?;
    run_cli(
        "Keeping generated files",
        args.json,
        sink,
        global,
        move |engine, rs| async move { keep(&engine, &rs, &paths, &cwd).await },
    )
}

pub fn execute_discard(
    args: &DiscardArgs,
    sink: LogSink,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let opts = Options {
        selection: if args.all {
            Selection::All
        } else {
            Selection::Paths(args.paths.clone())
        },
        cwd: cwd()?,
        dry_run: args.dry_run,
        force: args.force,
    };
    run_cli(
        "Discarding generated files",
        args.json,
        sink,
        global,
        move |engine, rs| async move { discard(&engine, &rs, &opts).await },
    )
}
