//! `heph tool codegen gitignore [--check] [MATCHER]`: rewrite the managed
//! section from the defs, and nothing else.

use crate::tui::session::SessionSlot;
use std::path::PathBuf;
use std::sync::Arc;

use serde::ser::SerializeStruct as _;

use super::{Report, blocking, display, read_gitignore, run_cli, write_gitignore};
use crate::commands::GlobalOptions;
use crate::engine::gitignore::{self, GitignoreEntry, StaleReason};
use crate::engine::request_state::RequestState;
use crate::engine::{Discovery, Engine, get_cwp};
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::tui::LogSink;
use hwalk::codegen_index::Section;

#[derive(clap::Args, Clone, Debug, Default)]
#[command(
    override_usage = "heph tool codegen gitignore [--check] [--json]\n       heph tool codegen gitignore [--check] [--json] <PACKAGE_MATCHER>"
)]
pub struct Args {
    /// Write nothing; exit 1 when the section is stale
    #[arg(long)]
    pub check: bool,
    #[command(flatten)]
    pub out: output::JsonArgs<GitignoreReport>,
    /// Package matcher (e.g. //pkg/...); only lines from targets under it
    /// change. Omit to rewrite the whole section.
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub matcher: Option<String>,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
}

/// What the command is asked to do, parsed.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub check: bool,
    /// A package matcher: only lines from targets under it change.
    pub matcher: Option<Matcher>,
}

/// A section line the rewrite adds or removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineChange {
    pub added: bool,
    pub pattern: String,
    /// The target that emits (or emitted) it; `None` for a line heph did not
    /// write.
    pub addr: Option<String>,
}

/// A line as `--json` prints it: `{op, pattern, addr}`, `op` being `add` or
/// `remove`.
impl serde::Serialize for LineChange {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_struct("LineChange", 3)?;
        m.serialize_field("op", if self.added { "add" } else { "remove" })?;
        m.serialize_field("pattern", &self.pattern)?;
        m.serialize_field("addr", &self.addr)?;
        m.end()
    }
}

/// A recorded file the rewrite releases: no line covers it any more.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Released {
    pub path: String,
    pub addr: String,
}

/// What `gitignore` did; under `--json`, the record it prints.
#[derive(Debug, Clone, serde::Serialize, output::Record)]
pub struct GitignoreReport {
    #[serde(skip)]
    pub check: bool,
    /// Whether the section differed from what the targets emit.
    pub stale: bool,
    /// Why it was stale: a sorted subset of `lines`, `malformed` and
    /// `old_marker`, empty when current.
    #[serde(serialize_with = "reason_strs")]
    pub reasons: Vec<StaleReason>,
    /// Whether `.gitignore` was rewritten; always false under `--check`.
    pub written: bool,
    /// The section lines added or removed, `{op, pattern, addr}`, sorted by
    /// pattern, then target, then op.
    pub lines: Vec<LineChange>,
    /// The recorded files no line covers any more, `{path, addr}`, sorted by
    /// path.
    pub released: Vec<Released>,
    /// How many lines the section holds after the rewrite.
    #[serde(skip)]
    pub entries: usize,
    #[serde(skip)]
    pub gitignore: PathBuf,
}

fn reason_strs<S: serde::Serializer>(reasons: &[StaleReason], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(reasons.iter().map(|r| r.as_str()))
}

/// The message for a section whose BEGIN line predates this heph.
pub fn old_marker_message() -> String {
    format!(
        "section marker predates heph {}; run heph tool codegen gitignore once and commit",
        crate::version::reported()
    )
}

/// One line per stale reason, for `--check` and `validate`.
pub fn stale_messages(reasons: &[StaleReason]) -> Vec<String> {
    reasons
        .iter()
        .map(|r| match r {
            StaleReason::OldMarker => old_marker_message(),
            StaleReason::Malformed => {
                ".gitignore has heph section markers but no well-formed section; run heph tool codegen gitignore".to_string()
            }
            StaleReason::Lines => {
                "the heph section of .gitignore is out of date; run heph tool codegen gitignore"
                    .to_string()
            }
        })
        .collect()
}

impl Report for GitignoreReport {
    fn text(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .lines
            .iter()
            .map(|l| {
                let op = if l.added { '+' } else { '-' };
                match &l.addr {
                    Some(a) => format!("{op} {}  ({a})", l.pattern),
                    None => format!("{op} {}", l.pattern),
                }
            })
            .collect();
        for r in &self.released {
            out.push(format!(
                "released {} (was {}): now untracked; keep or discard it",
                r.path, r.addr
            ));
        }
        let path = self.gitignore.display();
        if !self.stale {
            out.push(format!("{path} already up to date"));
        } else if self.check {
            out.extend(stale_messages(&self.reasons));
        } else {
            out.push(format!(
                "Updated {path} ({} entr{})",
                self.entries,
                if self.entries == 1 { "y" } else { "ies" }
            ));
        }
        out
    }

    fn exit_code(&self) -> u8 {
        u8::from(self.check && self.stale)
    }
}

/// Rewrite (or, with `check`, diff) the managed section. Resolves every def
/// before taking any codegen lock (J9), then holds the section lock for one
/// read, change and write of `.gitignore`, then marks `seen_in_section` on
/// the rows the new section covers, each target under its own lock.
pub async fn gitignore(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    opts: &Options,
) -> anyhow::Result<GitignoreReport> {
    engine.open_codegen_index(rs).await?;
    let walk_matcher = opts
        .matcher
        .clone()
        .unwrap_or_else(|| Matcher::TreeOutputTo(PkgBuf::from("")));
    let fresh = Arc::clone(engine)
        .codegen_copy_targets(rs.clone(), &walk_matcher, Discovery::Complete, false)
        .await?
        .entries();

    let root = engine.root().to_path_buf();
    let path = root.join(".gitignore");
    let section_lock = engine.codegen_locks().section(rs.ctoken()).await?;
    let (existing, diff) = blocking({
        let (root, matcher, check) = (root.clone(), opts.matcher.clone(), opts.check);
        move || {
            let _held = section_lock;
            let existing = read_gitignore(&root)?;
            let entries: Vec<GitignoreEntry> = match &matcher {
                Some(m) => gitignore::merge_section(&existing, fresh, m),
                None => fresh,
            };
            let diff = gitignore::diff_section(&existing, &entries);
            if !check && diff.is_stale() {
                write_gitignore(&root, &diff.rendered)?;
            }
            Ok((existing, diff))
        }
    })
    .await?;

    let index = Arc::clone(engine.codegen_index());
    let (released, newly_seen) = blocking({
        let rendered = diff.rendered.clone();
        move || {
            let old = Section::compile(&existing);
            let new = Section::compile(&rendered);
            let snap = index.snapshot()?;
            let mut released = Vec::new();
            for row in gitignore::released_rows(snap.rows(), &old, &new) {
                let abs = index
                    .root()
                    .join(hwalk::codegen_index::paths::rel_path(&row.path));
                if hwalk::codegen_index::fs::lstat_opt(index.fs().as_ref(), &abs)?.is_some() {
                    released.push(Released {
                        path: display(&row.path),
                        addr: row.addr.clone(),
                    });
                }
            }
            Ok((released, gitignore::newly_seen(snap.rows(), &new)))
        }
    })
    .await?;

    // The rows the written section covers are seen; a `--check` wrote
    // nothing, so it marks nothing.
    if !opts.check {
        for (addr, paths) in newly_seen {
            let guard = engine.codegen_locks().target(&addr, rs.ctoken()).await?;
            let index = Arc::clone(engine.codegen_index());
            blocking(move || index.mark_seen_in_section(&guard, &paths)).await?;
        }
    }

    let mut lines: Vec<LineChange> = diff
        .added
        .iter()
        .map(|e| (true, e))
        .chain(diff.removed.iter().map(|e| (false, e)))
        .map(|(added, e)| LineChange {
            added,
            pattern: e.pattern.clone(),
            addr: e.addr.as_ref().map(crate::htaddr::Addr::format),
        })
        .collect();
    lines.sort_by(|a, b| (&a.pattern, &a.addr, a.added).cmp(&(&b.pattern, &b.addr, b.added)));
    let entries = hwalk::gitignore_section::parse_section_bytes(&diff.rendered)
        .lines
        .len();
    Ok(GitignoreReport {
        check: opts.check,
        stale: diff.is_stale(),
        written: !opts.check && diff.is_stale(),
        reasons: diff.reasons,
        lines,
        released,
        entries,
        gitignore: path,
    })
}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let matcher = match &args.matcher {
        Some(s) => Some(htpkg::parse(s, &get_cwp()?)?),
        None => None,
    };
    let opts = Options {
        check: args.check,
        matcher,
    };
    let title = if args.check {
        "Checking .gitignore"
    } else {
        "Generating .gitignore"
    };
    run_cli(
        title,
        &args.out,
        args.dashboard.dashboard,
        sink,
        session,
        global,
        move |engine, rs| async move { gitignore(&engine, &rs, &opts).await },
    )
}
