//! `heph tool codegen status [--check]`: what needs a decision, one line per
//! file, each ending with the command that settles it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::json;

use super::{JSON_SCHEMA, Report, blocking, display, empty_index_hint, read_gitignore, run_cli};
use crate::commands::GlobalOptions;
use crate::engine::gitignore::{self, CopyTargets};
use crate::engine::query::skip_unresolvable;
use crate::engine::request_state::RequestState;
use crate::engine::{CopyOutputs, Discovery, Engine, Gaps};
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::tui::LogSink;
use hwalk::codegen_index::{LookupMeta, RowState, StatusKind};

#[derive(clap::Args, Clone, Debug, Default)]
pub struct Args {
    /// Exit 1 when anything is listed
    #[arg(long)]
    pub check: bool,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub check: bool,
    /// Settle pending rows from a fresh hash, each target under its lock.
    /// `validate` reads without settling.
    pub settle: bool,
    /// Compute the `codegen import` hint: on an empty index, a walk of the
    /// tree counting the files the section matches. `validate` leaves it
    /// off, so a fresh checkout walks nothing (Lifecycle row 22).
    pub hint: bool,
}

impl Options {
    /// What `validate` lists its codegen findings with: no settling, no
    /// hint walk.
    pub const VALIDATE: Self = Self {
        check: false,
        settle: false,
        hint: false,
    };
}

/// What a listed item needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemKind {
    /// A dropped or released row, or one its target no longer declares.
    NoLongerGenerated,
    /// The row's target no longer exists.
    TargetMissing,
    /// The bytes differ from what heph wrote.
    Modified,
    /// A write-back was interrupted: the row is still pending.
    Interrupted,
    /// A recorded file no section line covers.
    NotInSection,
    /// A section line heph did not write.
    ForeignLine,
    /// A package whose BUILD file failed to evaluate.
    Unresolved,
    /// The section differs from what the targets emit.
    SectionStale,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoLongerGenerated => "no_longer_generated",
            Self::TargetMissing => "target_missing",
            Self::Modified => "modified",
            Self::Interrupted => "interrupted",
            Self::NotInSection => "not_in_section",
            Self::ForeignLine => "foreign_line",
            Self::Unresolved => "unresolved",
            Self::SectionStale => "section_stale",
        }
    }
}

/// One listed item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusItem {
    pub kind: ItemKind,
    /// `None` for `unresolved`, which carries `package` and `error`.
    pub path: Option<String>,
    pub addr: Option<String>,
    pub output: Option<String>,
    /// The command (or action) that settles it.
    pub fix: String,
    /// The human description, between the path and the fix.
    pub detail: String,
    pub package: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StatusReport {
    pub check: bool,
    /// Sorted by path (an `unresolved` item by its package), then kind.
    pub items: Vec<StatusItem>,
    /// The `codegen import` hint (Upgrade).
    pub hint: Option<String>,
}

impl Report for StatusReport {
    fn text(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .items
            .iter()
            .map(|i| {
                let head = i.path.as_deref().or(i.package.as_deref()).unwrap_or("");
                format!("{head}  {}  {}", i.detail, i.fix)
            })
            .collect();
        if let Some(h) = &self.hint {
            out.push(h.clone());
        }
        if self.items.is_empty() && self.hint.is_none() {
            out.push("nothing needs a decision".to_string());
        }
        out
    }

    fn json(&self) -> serde_json::Value {
        let items: Vec<serde_json::Value> = self
            .items
            .iter()
            .map(|i| {
                if i.kind == ItemKind::Unresolved {
                    json!({
                        "kind": i.kind.as_str(),
                        "package": i.package,
                        "error": i.error,
                        "fix": i.fix,
                    })
                } else {
                    json!({
                        "path": i.path,
                        "kind": i.kind.as_str(),
                        "addr": i.addr,
                        "output": i.output,
                        "fix": i.fix,
                    })
                }
            })
            .collect();
        let mut doc = json!({ "schema": JSON_SCHEMA, "items": items });
        if let (Some(h), Some(m)) = (&self.hint, doc.as_object_mut()) {
            m.insert("hint".into(), json!(h));
        }
        doc
    }

    fn exit_code(&self) -> u8 {
        u8::from(self.check && !self.items.is_empty())
    }
}

/// What a row's addr resolves to now.
#[derive(Debug)]
pub(super) enum AddrState {
    /// The target exists; its current `copy` outputs (maybe none).
    Live(CopyOutputs),
    /// No such target.
    Missing,
    /// Its package failed to resolve.
    Unresolved { package: String, error: String },
}

/// Resolve each addr in `addrs`: from `copy` when it declares a copy output,
/// else by its own def.
pub(super) async fn resolve_addrs(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    addrs: BTreeSet<String>,
    copy: &CopyTargets,
) -> anyhow::Result<BTreeMap<String, AddrState>> {
    let mut out = BTreeMap::new();
    let mut pending = Vec::new();
    for a in addrs {
        match copy.get(&a) {
            Some(def) => {
                out.insert(a, AddrState::Live(CopyOutputs::of(def)?));
            }
            None => pending.push(a),
        }
    }
    let futs = pending.into_iter().map(|a| {
        let (engine, rs) = (Arc::clone(engine), Arc::clone(rs));
        async move {
            let state = match crate::htaddr::parse_addr(&a) {
                Err(e) => AddrState::Unresolved {
                    package: a.clone(),
                    error: format!("{e:#}"),
                },
                Ok(addr) => {
                    let res = Arc::clone(&engine).get_def(rs, &addr).await;
                    match skip_unresolvable(&addr, res) {
                        Ok(Some(def)) => AddrState::Live(CopyOutputs::of(&def.target_def)?),
                        Ok(None) => AddrState::Missing,
                        Err(e) => AddrState::Unresolved {
                            package: format!("//{}", addr.package),
                            error: format!("{e:#}"),
                        },
                    }
                }
            };
            Ok::<_, anyhow::Error>((a, state))
        }
    });
    for (a, s) in futures::future::try_join_all(futs).await? {
        out.insert(a, s);
    }
    Ok(out)
}

fn keep_or_discard(path: &str) -> String {
    format!("keep: heph tool codegen keep {path}  discard: heph tool codegen discard {path}")
}

/// Compute what needs a decision. With `settle`, pending rows are settled
/// first, each target under its own lock; defs are resolved before any lock
/// (J9). A broken BUILD file never fails it: its package is listed as
/// `unresolved` and its rows still appear under their other kinds.
pub async fn status(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    opts: Options,
) -> anyhow::Result<StatusReport> {
    engine.open_codegen_index(rs).await?;
    let gaps = Gaps::new("codegen status");
    let copy = Arc::clone(engine)
        .codegen_copy_targets(
            rs.clone(),
            &Matcher::TreeOutputTo(PkgBuf::from("")),
            Discovery::keep_going_unless(false, &gaps),
            true,
        )
        .await?;

    let index = Arc::clone(engine.codegen_index());
    let (addrs, pending_addrs) = blocking({
        let index = Arc::clone(&index);
        move || {
            let snap = index.snapshot()?;
            let mut addrs = BTreeSet::new();
            let mut pending = BTreeSet::new();
            for r in snap.rows() {
                addrs.insert(r.addr.clone());
                if r.pending_hash.is_some() {
                    pending.insert(r.addr.clone());
                }
            }
            Ok((addrs, pending))
        }
    })
    .await?;
    let states = resolve_addrs(engine, rs, addrs, &copy).await?;

    if opts.settle {
        for addr in pending_addrs {
            let guard = engine.codegen_locks().target(&addr, rs.ctoken()).await?;
            let index = Arc::clone(&index);
            blocking(move || index.settle_pending(&guard)).await?;
        }
    }

    let mut unresolved: BTreeMap<String, String> = BTreeMap::new();
    for g in gaps.report().groups {
        for ex in g.examples {
            unresolved.entry(ex.scope).or_insert(ex.cause);
        }
    }
    for (addr, e) in &copy.failed {
        unresolved
            .entry(format!("//{}", addr.package))
            .or_insert_with(|| format!("{e:#}"));
    }
    for s in states.values() {
        if let AddrState::Unresolved { package, error } = s {
            unresolved
                .entry(package.clone())
                .or_insert_with(|| error.clone());
        }
    }
    // Only a complete set of lines can tell a stale section.
    let entries = unresolved.is_empty().then(|| copy.entries());

    let mut items = blocking({
        let index = Arc::clone(&index);
        let root = engine.root().to_path_buf();
        move || {
            let snap = index.snapshot()?;
            let mut items = Vec::new();
            for row in snap.rows() {
                let path = display(&row.path);
                let addr = row.addr.clone();
                let mk = |kind, detail: String, fix: String| StatusItem {
                    kind,
                    path: Some(path.clone()),
                    addr: Some(addr.clone()),
                    output: row.output.clone(),
                    fix,
                    detail,
                    package: None,
                    error: None,
                };
                if row.pending_hash.is_some() {
                    items.push(mk(
                        ItemKind::Interrupted,
                        format!("a write-back of {addr} was interrupted"),
                        format!("heph run {addr}"),
                    ));
                    continue;
                }
                let state = states.get(&addr);
                if matches!(state, Some(AddrState::Missing)) {
                    items.push(mk(
                        ItemKind::TargetMissing,
                        format!("generated by {addr}, which no longer exists"),
                        keep_or_discard(&path),
                    ));
                    continue;
                }
                let st = snap.status(&row.path, &LookupMeta::point(false))?;
                let declared = match state {
                    Some(AddrState::Live(outs)) => Some(outs.contains(&row.path)),
                    _ => None,
                };
                if st.kind == StatusKind::Modified {
                    items.push(mk(
                        ItemKind::Modified,
                        format!("edited since {addr} wrote it"),
                        format!(
                            "rebuild {addr} to overwrite it, or remove the output and keep it: heph tool codegen keep {path}"
                        ),
                    ));
                    continue;
                }
                if row.state == RowState::Dropped
                    || st.kind == StatusKind::Released
                    || declared == Some(false)
                {
                    items.push(mk(
                        ItemKind::NoLongerGenerated,
                        format!("no longer generated (was {addr})"),
                        keep_or_discard(&path),
                    ));
                    continue;
                }
                let covered = snap.section().match_point(&row.path, false).is_territory();
                if !covered && snap.section().is_present() {
                    items.push(mk(
                        ItemKind::NotInSection,
                        format!("written by {addr}, but no line of the heph section covers it"),
                        "heph tool codegen gitignore".to_string(),
                    ));
                }
            }
            for line in snap.section().lines() {
                if line.is_comment() || !line.is_foreign() {
                    continue;
                }
                items.push(StatusItem {
                    kind: ItemKind::ForeignLine,
                    path: Some(line.entry.pattern.clone()),
                    addr: None,
                    output: None,
                    fix: "heph tool codegen gitignore".to_string(),
                    detail: format!(
                        ".gitignore:{} is in the heph section, but no target wrote it",
                        line.line_no
                    ),
                    package: None,
                    error: None,
                });
            }
            if let Some(entries) = entries {
                let existing = read_gitignore(&root)?;
                let diff = gitignore::diff_section(&existing, &entries);
                if diff.is_stale() {
                    items.push(StatusItem {
                        kind: ItemKind::SectionStale,
                        path: Some(".gitignore".to_string()),
                        addr: None,
                        output: None,
                        fix: "heph tool codegen gitignore".to_string(),
                        detail: super::gitignore::stale_messages(&diff.reasons).join("; "),
                        package: None,
                        error: None,
                    });
                }
            }
            Ok(items)
        }
    })
    .await?;
    for (package, error) in unresolved {
        items.push(StatusItem {
            kind: ItemKind::Unresolved,
            path: None,
            addr: None,
            output: None,
            fix: "fix the BUILD file".to_string(),
            detail: format!("failed to resolve: {error}"),
            package: Some(package),
            error: Some(error),
        });
    }
    items.sort_by(|a, b| {
        let ka = (a.path.as_ref().or(a.package.as_ref()), a.kind);
        let kb = (b.path.as_ref().or(b.package.as_ref()), b.kind);
        ka.cmp(&kb)
    });

    let hint = if opts.hint {
        empty_index_hint(engine).await?
    } else {
        None
    };
    Ok(StatusReport {
        check: opts.check,
        items,
        hint,
    })
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    let opts = Options {
        check: args.check,
        settle: true,
        hint: true,
    };
    run_cli(
        "Codegen status",
        args.json,
        sink,
        global,
        move |engine, rs| async move { status(&engine, &rs, opts).await },
    )
}
