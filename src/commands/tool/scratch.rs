//! `heph tool scratch …` — inspect and reclaim persistent cache directories.
//!
//! Sits in `heph tool` alongside `cache`, `gc` and `clean`, which is the group for
//! commands that inspect or repair heph's own state rather than build anything.
//! A scratch slot is exactly that kind of state.
//!
//! Everything here reads the store directly and resolves no BUILD files, so it
//! keeps working when the targets that produced a slot have been deleted or
//! renamed — the same property `heph tool clean` has for addr-only selections.

use crate::tui::session::SessionSlot;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;

use crate::commands::progress_app::{DashboardArgs, request_state, run_with_progress_out};
use crate::commands::{GlobalOptions, bootstrap};
use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use crate::engine::scratch_remote::RemoteHead;
use crate::engine::scratch_store::{SlotEntry, head_dir, store_root};
use crate::htaddr::Addr;
use crate::tui::{AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct ScratchArgs {
    #[command(subcommand)]
    pub command: ScratchCommands,
}

#[derive(clap::Subcommand, Clone)]
pub enum ScratchCommands {
    /// List persistent scratch cache directories
    ///
    /// One row per declared cache, largest first, with the lineages (branches)
    /// present and the total size on disk. Reads no BUILD files, so a cache whose
    /// declaring target is gone still shows up — and can still be removed.
    ///
    /// Example: `heph tool scratch ls`
    Ls {
        #[command(flatten)]
        out: output::JsonArgs<LsRow<'static>>,
    },
    /// Explain which lineage a cache would be restored from
    ///
    /// Prints the resolution walk: every candidate lineage in the order it is
    /// consulted, what each holds, and which one wins. This is the answer to
    /// "why did my branch build start cold?" — a question the local directory
    /// alone cannot answer, because the interesting part is what was *not* found.
    ///
    /// Example: `heph tool scratch head //build:gocache`
    Head {
        /// Address of the `scratch` target.
        addr: String,
        #[command(flatten)]
        dashboard: DashboardArgs,
        #[command(flatten)]
        out: output::JsonArgs<HeadView<'static>>,
    },
    /// Print the on-disk path of a scratch cache
    ///
    /// For pointing another tool at it, or `du`-ing it by hand.
    ///
    /// Example: `heph tool scratch path //build:gocache`
    Path {
        /// Address of the `scratch` target, e.g. `//build:gocache`.
        addr: String,
        #[command(flatten)]
        out: output::JsonArgs<PathView<'static>>,
    },
    /// Publish scratch caches to the remote
    ///
    /// The command CI runs as its last step. Publishing is **never** a side
    /// effect of building: it is expensive, it mutates shared state, and whether
    /// a given job's cache state deserves to become the branch's published head
    /// is a CI-policy question — one answered far better by an `if:` condition in
    /// a workflow than by a heuristic inside heph.
    ///
    /// Writes into the current branch's lineage and never into a fallback, even
    /// the one the cache was seeded from. That isolation is what makes this safe
    /// to enable on untrusted PR CI at all.
    ///
    /// Example: `heph tool scratch push --all --producer "$GITHUB_RUN_ID"`
    Push {
        /// Address of the `scratch` target to publish.
        addr: Option<String>,
        /// Publish every cache declared `remote = True`.
        #[arg(long)]
        all: bool,
        /// Publish even when the contents are identical to what is already there.
        #[arg(long)]
        force: bool,
        /// Free-form producer id recorded with the snapshot, e.g. a CI run id.
        #[arg(long, default_value = "")]
        producer: String,
        #[command(flatten)]
        out: output::JsonArgs<PushRow<'static>>,
    },
    /// Fetch scratch caches from the remote without building
    ///
    /// Builds do this on their own when a cache is cold, so this is for warming a
    /// machine ahead of time, or recovering after a local cache went bad.
    ///
    /// Example: `heph tool scratch pull --all`
    Pull {
        /// Address of the `scratch` target to fetch.
        addr: Option<String>,
        /// Fetch every cache declared `remote = True`.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        dashboard: DashboardArgs,
        #[command(flatten)]
        out: output::JsonArgs<PullRow<'static>>,
    },
    /// Delete scratch cache directories
    ///
    /// The remedy when a cache has gone bad: the next build starts cold and
    /// repopulates it. Deleting a scratch is always safe — a target's outputs are
    /// identical whether its scratch is warm, cold or absent, so this costs time
    /// and nothing else.
    ///
    /// Examples:
    ///
    /// `heph tool scratch rm //build:gocache` — one cache
    ///
    /// `heph tool scratch rm --all` — every cache
    Rm {
        /// Address of the `scratch` target to delete.
        addr: Option<String>,
        /// Delete every scratch cache in this workspace.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        out: output::JsonArgs<RmView>,
    },
}

/// `println!` into a [`BufferedStdout`], so a line printed mid-walk never lands
/// inside a live TUI frame.
macro_rules! outln {
    ($out:expr, $($fmt:tt)*) => {
        $out.println(format!($($fmt)*))
    };
}

impl ScratchArgs {
    /// `head` and `pull` resolve every spec in the workspace to find the
    /// declarations, and a provider may run a target to answer that — so they
    /// run under the TUI like `run` does. The rest read the store, or the remote,
    /// and build nothing.
    pub fn execute(
        &self,
        sink: LogSink,
        session: &Arc<SessionSlot>,
        global: &GlobalOptions,
    ) -> anyhow::Result<()> {
        match &self.command {
            ScratchCommands::Ls { out } => bootstrap::block_on(ls(out, session, global))?,
            ScratchCommands::Head {
                addr,
                dashboard,
                out,
            } => bootstrap::block_on(head(
                addr.clone(),
                dashboard.dashboard,
                out.clone(),
                sink,
                session,
                global.clone(),
            ))?,
            ScratchCommands::Path { addr, out } => {
                bootstrap::block_on(path(addr, out, session, global))?
            }
            ScratchCommands::Rm { addr, all, out } => {
                bootstrap::block_on(rm(addr.as_deref(), *all, out, session, global))?
            }
            ScratchCommands::Push {
                addr,
                all,
                force,
                producer,
                out,
            } => bootstrap::block_on(push(
                addr.as_deref(),
                *all,
                *force,
                producer.clone(),
                out,
                session,
                global,
            ))?,
            ScratchCommands::Pull {
                addr,
                all,
                dashboard,
                out,
            } => {
                if *all == addr.is_some() {
                    anyhow::bail!("pass either an address or --all, not both or neither");
                }
                bootstrap::block_on(pull(
                    addr.clone(),
                    dashboard.dashboard,
                    out.clone(),
                    sink,
                    session,
                    global.clone(),
                ))?
            }
        }
    }
}

/// Name a lineage for a human. The default lineage has an empty name, and
/// printing `` for it reads like something went wrong.
fn describe_scope(scope: &str) -> String {
    if scope.is_empty() {
        "the default lineage".to_string()
    } else {
        format!("`{scope}`")
    }
}

fn describe(slot: &SlotEntry) -> (String, String) {
    match &slot.meta {
        Some(m) => {
            // A mountless cache has no path, and `exclusive @ ` with nothing
            // after it reads like a truncated line rather than a deliberate
            // absence. The env-var-only form is the common one, so this is not
            // a rare case worth tolerating.
            let mut detail = if m.path.is_empty() {
                format!("{} (env {})", m.access, m.env)
            } else {
                format!("{} @ {}", m.access, m.path)
            };
            if !m.version.is_empty() {
                detail.push_str(&format!(" v={}", m.version));
            }
            if m.remote {
                detail.push_str(" remote");
            }
            (m.addr.clone(), detail)
        }
        // An orphan: a slot from a format heph no longer reads, or one whose meta
        // never landed. Say so rather than hiding it — it is occupying disk and
        // `rm --all` is how it goes.
        None => (format!("<unknown> ({})", slot.slot), String::new()),
    }
}

/// `heph tool scratch ls --json`: one element per cache on disk, largest
/// first; `[]` when there is none.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct LsRow<'a> {
    /// The declaring `scratch` target; `null` for an orphan slot, whose
    /// description is missing or unreadable.
    addr: Option<&'a str>,
    /// The slot's directory name in the scratch store.
    slot: &'a str,
    /// Bytes on disk, across every lineage.
    bytes: u64,
    /// The lineages present, by their directory name in the slot.
    lineages: &'a [String],
    /// `exclusive` or `shared`; `null` for an orphan.
    access: Option<&'a str>,
    /// Where the cache is mounted in a consumer's sandbox; `""` when it is
    /// announced only through `env`; `null` for an orphan.
    path: Option<&'a str>,
    /// The environment variable the cache is announced through; `null` for an
    /// orphan.
    env: Option<&'a str>,
    /// The declared `version`; `""` when none is; `null` for an orphan.
    version: Option<&'a str>,
    /// Whether the cache is declared `remote = True`; `null` for an orphan.
    remote: Option<bool>,
}

impl<'a> LsRow<'a> {
    fn new(slot: &'a SlotEntry) -> Self {
        let meta = slot.meta.as_ref();
        Self {
            addr: meta.map(|m| m.addr.as_str()),
            slot: &slot.slot,
            bytes: slot.bytes,
            lineages: &slot.scopes,
            access: meta.map(|m| m.access.as_str()),
            path: meta.map(|m| m.path.as_str()),
            env: meta.map(|m| m.env.as_str()),
            version: meta.map(|m| m.version.as_str()),
            remote: meta.map(|m| m.remote),
        }
    }
}

async fn ls(
    out: &output::JsonArgs<LsRow<'static>>,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global, session)?;
    let mut slots = engine.scratch_slots()?;
    // Largest first: the reason to run this is usually disk.
    slots.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    if out.enabled() {
        return out.list_iter(slots.iter().map(LsRow::new), &output::never_cancelled);
    }

    let mut stdout = std::io::stdout().lock();
    if slots.is_empty() {
        writeln!(stdout, "No scratch caches in this workspace.").context("writing the list")?;
        return Ok(());
    }
    let total: u64 = slots.iter().map(|s| s.bytes).sum();
    let width = slots
        .iter()
        .map(|s| describe(s).0.len())
        .max()
        .unwrap_or(0)
        .max(7);

    writeln!(stdout, "{:<width$}  {:>10}  LINEAGES", "CACHE", "SIZE")
        .context("writing the list")?;
    for slot in &slots {
        let (name, detail) = describe(slot);
        let scopes = if slot.scopes.is_empty() {
            "-".to_string()
        } else {
            slot.scopes.join(", ")
        };
        writeln!(
            stdout,
            "{name:<width$}  {:>10}  {scopes}",
            hcore::units::human_bytes(slot.bytes)
        )
        .context("writing the list")?;
        if !detail.is_empty() {
            writeln!(stdout, "{:<width$}  {:>10}  {detail}", "", "").context("writing the list")?;
        }
    }
    writeln!(stdout).context("writing the list")?;
    writeln!(
        stdout,
        "{} cache(s), {} total",
        slots.len(),
        hcore::units::human_bytes(total)
    )
    .context("writing the list")?;
    Ok(())
}

/// The local lineages a build would consult, in order, each with whether it is
/// warm. Stops at the first warm one, exactly as
/// `engine::scratch::resolve_scope_dir` does — this walk is an explanation of
/// that one, so any divergence is a lie rather than a cosmetic difference.
fn local_trace(
    home: &Path,
    slot: &str,
    scope: &str,
    fallbacks: &[String],
    seed_on_fork: bool,
) -> Vec<(String, bool)> {
    let has = |sc: &str| {
        let p = head_dir(home, slot, sc);
        std::fs::read_dir(&p).is_ok_and(|mut d| d.next().is_some())
    };

    let own = has(scope);
    let mut out = vec![(scope.to_string(), own)];
    if own || !seed_on_fork {
        return out;
    }
    for fb in fallbacks {
        // The own scope is already reported; a fallback repeating it is not a
        // second chance.
        if fb == scope {
            continue;
        }
        let warm = has(fb);
        out.push((fb.clone(), warm));
        if warm {
            break;
        }
    }
    out
}

/// What `head` found: the local walk, then the remote one when it was
/// consulted. Both renderings — the text and the JSON record — read this, so
/// they cannot disagree about what was found.
struct HeadTrace {
    addr: Addr,
    slot: String,
    /// The current lineage.
    scope: String,
    /// The local candidates, in the order a build consults them, up to the
    /// first warm one ([`local_trace`]).
    local: Vec<LocalCandidate>,
    remote: RemoteTrace,
}

struct LocalCandidate {
    scope: String,
    warm: bool,
    /// Where this lineage's head lives on this machine.
    dir: PathBuf,
}

/// The remote half of a [`HeadTrace`].
enum RemoteTrace {
    /// Not consulted: the cache declares `remote = False`.
    DeclaredLocal,
    /// Not consulted: no remote cache is configured.
    NoRemoteCache,
    /// Each candidate lineage, in order, with its best head if it has one.
    Consulted(Vec<(String, Option<RemoteHead>)>),
}

impl HeadTrace {
    /// Whether a local lineage is warm: resolution stops there, and the remote
    /// is shown for context only.
    fn warm_locally(&self) -> bool {
        self.local.last().is_some_and(|c| c.warm)
    }

    /// The remote head a build here restores from: the first lineage with
    /// anything, unless a local one is warm.
    fn remote_winner(&self) -> Option<(&str, &RemoteHead)> {
        if self.warm_locally() {
            return None;
        }
        match &self.remote {
            RemoteTrace::Consulted(trace) => trace
                .iter()
                .find_map(|(sc, h)| h.as_ref().map(|h| (sc.as_str(), h))),
            RemoteTrace::DeclaredLocal | RemoteTrace::NoRemoteCache => None,
        }
    }

    /// The human walk, one line per element. `home` is where a head lives on
    /// this machine, for the absolute-path warning.
    fn render(&self, home: &Path) -> Vec<String> {
        let scope = &self.scope;
        let mut lines = vec![format!("{}  (slot {})", self.addr, self.slot)];

        // Local first, and with its *own* fallbacks — a build seeds a cold
        // lineage from a warm sibling before it ever looks at the remote, so a
        // trace that skipped straight to the network would name the wrong
        // winner in exactly the case this command exists for: a fresh branch
        // on a machine that has built its base.
        for c in &self.local {
            if !c.warm {
                lines.push(format!("    local {}: cold", describe_scope(&c.scope)));
            } else if c.scope == *scope {
                lines.push(format!(
                    "  * local {}: warm — {}",
                    describe_scope(&c.scope),
                    c.dir.display()
                ));
            } else {
                lines.push(format!(
                    "  * local {}: warm — seeds {} from {}",
                    describe_scope(&c.scope),
                    describe_scope(scope),
                    c.dir.display()
                ));
            }
        }
        let warm = self.warm_locally();
        if warm {
            // Resolution stops here; everything below is printed for context,
            // not as a prediction. Saying so beats letting the remote section
            // imply a fetch that will not happen.
            lines.push("  (warm locally — a build here does not consult the remote)".to_string());
        }

        let trace = match &self.remote {
            RemoteTrace::DeclaredLocal => {
                lines.push(
                    "  remote: not consulted (this cache declares `remote = False`)".to_string(),
                );
                return lines;
            }
            RemoteTrace::NoRemoteCache => {
                lines.push("  remote: no remote cache is configured".to_string());
                return lines;
            }
            RemoteTrace::Consulted(trace) => trace,
        };

        let mut winner_shown = false;
        for (sc, head) in trace {
            let Some(h) = head else {
                lines.push(format!(
                    "    remote {}: nothing published",
                    describe_scope(sc)
                ));
                continue;
            };
            // The first scope with anything is the one a build takes; the rest
            // are printed so the ordering is visible rather than implied.
            let wins = !winner_shown && !warm;
            winner_shown = true;
            // `bytes` is the unpacked size; `push` reports what it uploaded.
            // Saying which this is stops the two commands from looking like
            // they disagree about one snapshot.
            let mut line = format!(
                "  {} remote {}: generation {} ({} unpacked, from `{}`",
                if wins { "*" } else { " " },
                describe_scope(sc),
                h.meta.generation,
                hcore::units::human_bytes(h.meta.bytes),
                h.cache,
            );
            if !h.meta.producer.is_empty() {
                line.push_str(&format!(", producer {}", h.meta.producer));
            }
            line.push(')');
            lines.push(line);
            // Only for the entry that would actually be restored — the others
            // are never unpacked, so where they were produced says nothing
            // about this machine.
            // Compare against where *its own* lineage lives here: the tail
            // differs by scope on every cross-branch restore, which is
            // ordinary, so comparing full paths would warn constantly. A
            // difference against its own scope's path means a different
            // workspace or home — the case that actually breaks a cache whose
            // contents embed absolute paths.
            let native = head_dir(home, &self.slot, &h.meta.scope);
            if wins && h.meta.produced_at != native.to_string_lossy() {
                lines.push(format!(
                    "      produced under {}, restores under {} — a cache whose contents \
                     embed absolute paths will restore but be inert",
                    h.meta.produced_at,
                    head_dir(home, &self.slot, scope).display()
                ));
            }
        }
        if !winner_shown && !warm {
            // Only cold if the local head is cold too — resolution consults the
            // remote *because* local missed, so an empty remote with a warm
            // local is a perfectly good build.
            lines.push(
                "  nothing published in any candidate lineage — a build here starts cold"
                    .to_string(),
            );
        }
        lines
    }

    fn view(&self) -> HeadView<'_> {
        let (restores_from, restores_lineage) = if self.warm_locally() {
            (
                Some(RestoresFrom::Local),
                self.local.last().map(|c| c.scope.as_str()),
            )
        } else if let Some((sc, _)) = self.remote_winner() {
            (Some(RestoresFrom::Remote), Some(sc))
        } else {
            (None, None)
        };
        let (remote, remote_skipped) = match &self.remote {
            RemoteTrace::DeclaredLocal => (None, Some(RemoteSkipped::DeclaredLocal)),
            RemoteTrace::NoRemoteCache => (None, Some(RemoteSkipped::NoRemoteCache)),
            RemoteTrace::Consulted(trace) => (
                Some(
                    trace
                        .iter()
                        .map(|(sc, h)| RemoteView::new(sc, h.as_ref()))
                        .collect(),
                ),
                None,
            ),
        };
        HeadView {
            addr: &self.addr,
            slot: &self.slot,
            lineage: &self.scope,
            local: self
                .local
                .iter()
                .map(|c| LocalView {
                    lineage: &c.scope,
                    warm: c.warm,
                    path: &c.dir,
                })
                .collect(),
            remote,
            remote_skipped,
            restores_from,
            restores_lineage,
        }
    }
}

/// `heph tool scratch head --json`: the resolution walk, and what it settles on.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct HeadView<'a> {
    /// The `scratch` target.
    addr: &'a Addr,
    /// The cache's directory name in the scratch store.
    slot: &'a str,
    /// The lineage a build here uses (the branch's scope; `""` is the default
    /// lineage).
    lineage: &'a str,
    /// The local lineages a build consults, in order, up to the first warm
    /// one: `{lineage, warm, path}`.
    local: Vec<LocalView<'a>>,
    /// The remote lineages consulted, in order: `{lineage, generation, bytes,
    /// cache, producer, produced_at}`, all but `lineage` `null` when nothing
    /// is published there; `null` when the remote was not consulted.
    remote: Option<Vec<RemoteView<'a>>>,
    /// Why the remote was not consulted: `declared_local` (`remote = False`)
    /// or `no_remote_cache`; `null` when it was.
    remote_skipped: Option<RemoteSkipped>,
    /// Where a build here restores the cache from: `local` or `remote`; `null`
    /// when it starts cold.
    restores_from: Option<RestoresFrom>,
    /// The lineage it restores from; `null` when it starts cold.
    restores_lineage: Option<&'a str>,
}

#[derive(Debug, serde::Serialize, output::Shape)]
pub(crate) struct LocalView<'a> {
    lineage: &'a str,
    warm: bool,
    #[serde(serialize_with = "output::ser::path")]
    path: &'a Path,
}

#[derive(Debug, serde::Serialize, output::Shape)]
pub(crate) struct RemoteView<'a> {
    lineage: &'a str,
    /// How far along its lineage the published head is.
    generation: Option<u64>,
    /// Its unpacked size.
    bytes: Option<u64>,
    /// The remote cache it is in.
    cache: Option<&'a str>,
    /// The `--producer` it was published with; `null` when none was given.
    producer: Option<&'a str>,
    /// The absolute path it was produced at.
    produced_at: Option<&'a str>,
}

impl<'a> RemoteView<'a> {
    fn new(lineage: &'a str, head: Option<&'a RemoteHead>) -> Self {
        Self {
            lineage,
            generation: head.map(|h| h.meta.generation),
            bytes: head.map(|h| h.meta.bytes),
            cache: head.map(|h| h.cache.as_str()),
            producer: head
                .map(|h| h.meta.producer.as_str())
                .filter(|p| !p.is_empty()),
            produced_at: head.map(|h| h.meta.produced_at.as_str()),
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteSkipped {
    DeclaredLocal,
    NoRemoteCache,
}

#[derive(Debug, Clone, Copy, serde::Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestoresFrom {
    Local,
    Remote,
}

async fn head(
    addr: String,
    dashboard: bool,
    out: output::JsonArgs<HeadView<'static>>,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, shutdown) = bootstrap::new_engine(&global, session)?;
    let label = format!("Resolving scratch {addr}");
    run_with_progress_out(
        label,
        sink,
        session,
        global.no_tui,
        &out.clone(),
        dashboard,
        shutdown,
        move |ctx| head_body(engine, addr, global.fail_fast, out, ctx),
    )
    .await
}

async fn head_body(
    engine: Arc<Engine>,
    addr: String,
    fail_fast: bool,
    out: output::JsonArgs<HeadView<'static>>,
    ctx: AppContext,
) -> anyhow::Result<()> {
    let rs = request_state(&engine, &ctx);
    if out.enabled() {
        let trace = head_trace(&engine, rs.clone(), &addr, fail_fast, warn_skip).await?;
        let cancelled = || rs.ctoken().is_cancelled();
        return out.record(&trace.view(), &cancelled);
    }
    let text = BufferedStdout::new(&ctx);
    let res = head_trace(&engine, rs, &addr, fail_fast, |a, err| {
        text.println(skip_line(a, err))
    })
    .await
    .map(|trace| {
        for line in trace.render(&engine.home) {
            text.println(line);
        }
    });
    text.close().await;
    res
}

async fn head_trace(
    engine: &Arc<Engine>,
    rs: Arc<RequestState>,
    addr: &str,
    fail_fast: bool,
    report_skip: impl Fn(&Addr, &anyhow::Error),
) -> anyhow::Result<HeadTrace> {
    let gaps = crate::engine::Gaps::new("//...");
    let discovery = crate::engine::Discovery::keep_going_unless(fail_fast, &gaps);
    let found = declared_scratches(engine, rs, discovery, report_skip)
        .await?
        .into_iter()
        .find(|(a, _)| a.format() == addr);
    let Some((found_addr, def)) = found else {
        // It may be behind what the walk could not resolve.
        crate::commands::errors::require_complete_selection(&gaps)?;
        anyhow::bail!(
            "no `scratch` target named {addr}. `heph query -e '//...'` lists what the \
             workspace declares"
        );
    };

    let remote_declared = def.remote;
    let slot = crate::engine::scratch::ResolvedScratch {
        addr: found_addr.clone(),
        def,
    }
    .slot();
    let scope = engine.scratch_scope().to_string();
    let fallbacks = engine.scratch_restore_scopes().to_vec();

    let local = local_trace(
        &engine.home,
        &slot,
        &scope,
        &fallbacks,
        engine.scratch_seeds_on_fork(),
    )
    .into_iter()
    .map(|(sc, warm)| LocalCandidate {
        dir: head_dir(&engine.home, &slot, &sc),
        scope: sc,
        warm,
    })
    .collect();

    let remote = if !remote_declared {
        RemoteTrace::DeclaredLocal
    } else if engine.remote_caches().is_empty() {
        RemoteTrace::NoRemoteCache
    } else {
        RemoteTrace::Consulted(engine.scratch_remote_trace(&slot, &scope, &fallbacks).await)
    };
    Ok(HeadTrace {
        addr: found_addr,
        slot,
        scope,
        local,
        remote,
    })
}

/// `heph tool scratch path --json`.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct PathView<'a> {
    /// The `scratch` target.
    addr: &'a str,
    /// The cache's directory on disk.
    #[serde(serialize_with = "output::ser::path")]
    path: PathBuf,
}

async fn path(
    addr: &str,
    out: &output::JsonArgs<PathView<'static>>,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global, session)?;
    let slots = engine.scratch_slots()?;
    let found = slots
        .iter()
        .find(|s| s.meta.as_ref().is_some_and(|m| m.addr == addr));
    let Some(slot) = found else {
        // Not an error to be cold — a cache that has never been built has no
        // directory yet, and saying "no such thing" would be wrong.
        anyhow::bail!(
            "no scratch cache on disk for {addr}. It may simply never have been built; \
             `heph tool scratch ls` shows what is there"
        );
    };
    let path = store_root(&engine.home).join(&slot.slot);
    if out.enabled() {
        return out.record(&PathView { addr, path }, &output::never_cancelled);
    }
    writeln!(std::io::stdout(), "{}", path.display()).context("writing the path")
}

/// `heph tool scratch rm --json`.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct RmView {
    /// Caches removed.
    removed: usize,
    /// Bytes they held.
    bytes_removed: u64,
}

async fn rm(
    addr: Option<&str>,
    all: bool,
    out: &output::JsonArgs<RmView>,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    if all == addr.is_some() {
        anyhow::bail!("pass either an address or --all, not both or neither");
    }
    let (engine, _shutdown) = bootstrap::new_engine(global, session)?;
    let (removed, freed) = engine.scratch_remove(addr)?;
    if out.enabled() {
        let record = RmView {
            removed,
            bytes_removed: freed,
        };
        return out.record(&record, &output::never_cancelled);
    }
    if removed == 0 {
        writeln!(std::io::stdout(), "Nothing to remove.").context("writing the summary")
    } else {
        writeln!(
            std::io::stdout(),
            "Removed {removed} cache(s), freed {}.",
            hcore::units::human_bytes(freed)
        )
        .context("writing the summary")
    }
}

/// Pick the slots a `push`/`pull` selection names.
///
/// A slot with no readable meta is never selected: it cannot be named, and it
/// cannot say whether it is `remote` either, so publishing it would be a guess.
fn select(slots: Vec<SlotEntry>, addr: Option<&str>, all: bool) -> anyhow::Result<Vec<SlotEntry>> {
    if all == addr.is_some() {
        anyhow::bail!("pass either an address or --all, not both or neither");
    }
    Ok(slots
        .into_iter()
        .filter(|s| match (&s.meta, addr) {
            (Some(m), Some(want)) => m.addr == want,
            // `--all` means every cache that opted into travelling, not every
            // cache: a local-only one has nowhere to go.
            (Some(m), None) => m.remote,
            (None, _) => false,
        })
        .collect())
}

/// `heph tool scratch push --json`: one element per selected cache, in the
/// order they were published; `[]` when nothing is selected. The list is
/// whole even when a publish failed; the command then exits non-zero.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct PushRow<'a> {
    /// The `scratch` target.
    addr: &'a str,
    /// `published`, `unchanged` (this lineage already holds these bytes),
    /// `skipped` (nothing built in this lineage) or `failed`.
    outcome: PushOutcome,
    /// The lineage's generation now published; `null` when skipped or failed.
    generation: Option<u64>,
    /// Bytes uploaded; `null` unless published.
    bytes: Option<u64>,
    /// Why the publish failed; `null` unless it did.
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PushOutcome {
    Published,
    Unchanged,
    Skipped,
    Failed,
}

/// What publishing one cache did.
enum Pushed {
    /// Nothing built in this lineage.
    Skipped,
    /// The lineage already holds exactly these bytes, at this generation.
    Unchanged(u64),
    Published {
        generation: u64,
        bytes: u64,
    },
    Failed(anyhow::Error),
}

impl Pushed {
    fn line(&self, name: &str) -> String {
        match self {
            Pushed::Skipped => format!("{name}: nothing built in this lineage, skipped"),
            Pushed::Unchanged(_) => format!("{name}: unchanged, skipped"),
            Pushed::Published { generation, bytes } => {
                format!("{name}: published generation {generation} ({bytes} bytes)")
            }
            Pushed::Failed(err) => format!("{name}: FAILED — {err:#}"),
        }
    }

    fn row<'a>(&self, addr: &'a str) -> PushRow<'a> {
        let (outcome, generation, bytes, error) = match self {
            Pushed::Skipped => (PushOutcome::Skipped, None, None, None),
            Pushed::Unchanged(generation) => {
                (PushOutcome::Unchanged, Some(*generation), None, None)
            }
            Pushed::Published { generation, bytes } => (
                PushOutcome::Published,
                Some(*generation),
                Some(*bytes),
                None,
            ),
            Pushed::Failed(err) => (PushOutcome::Failed, None, None, Some(format!("{err:#}"))),
        };
        PushRow {
            addr,
            outcome,
            generation,
            bytes,
            error,
        }
    }
}

async fn push(
    addr: Option<&str>,
    all: bool,
    force: bool,
    producer: String,
    out: &output::JsonArgs<PushRow<'static>>,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global, session)?;
    let selected = select(engine.scratch_slots()?, addr, all)?;
    let mut list = out
        .enabled()
        .then(|| out.printer().start_list())
        .transpose()?;
    if selected.is_empty() && list.is_none() {
        writeln!(std::io::stdout(), "Nothing to publish.").context("writing the summary")?;
        return Ok(());
    }

    let scope = engine.scratch_scope().to_string();
    let mut failed = 0usize;
    for slot in &selected {
        let name = slot
            .meta
            .as_ref()
            .map_or(slot.slot.as_str(), |m| m.addr.as_str());
        let dir = head_dir(&engine.home, &slot.slot, &scope);
        let pushed = if dir.is_dir() {
            let parent =
                crate::engine::scratch_remote::read_local_meta(&engine.home, &slot.slot, &scope);
            let parent = if force { None } else { parent };
            match engine
                .scratch_push(&slot.slot, &scope, &dir, parent.as_ref(), &producer)
                .await
            {
                Ok((generation, 0)) => Pushed::Unchanged(generation),
                Ok((generation, bytes)) => Pushed::Published { generation, bytes },
                // Reported per slot and counted, rather than aborting: one
                // unpublishable cache should not silently drop the rest.
                Err(err) => {
                    failed += 1;
                    Pushed::Failed(err)
                }
            }
        } else {
            Pushed::Skipped
        };
        match &mut list {
            Some(list) => {
                // A closed stdout stops the publishing: nobody reads on.
                if list.push(&pushed.row(name))? == output::Flow::Stop
                    || list.flush()? == output::Flow::Stop
                {
                    break;
                }
            }
            None => writeln!(std::io::stdout(), "{}", pushed.line(name))
                .context("writing the report")?,
        }
    }
    if let Some(list) = list {
        // Whole even when a publish failed: every row is in it.
        list.finish(&output::never_cancelled)?;
    }
    if failed > 0 {
        anyhow::bail!("{failed} scratch cache(s) failed to publish");
    }
    Ok(())
}

/// The line `declared_scratches` reports a declaration it skipped with.
fn skip_line(addr: &Addr, err: &anyhow::Error) -> String {
    format!("{addr}: skipped — {err:#}")
}

/// Under the output flags stdout is the document's (I1): a skipped
/// declaration is a warning on stderr instead.
fn warn_skip(addr: &Addr, err: &anyhow::Error) {
    tracing::warn!(addr = %addr, error = %format!("{err:#}"), "skipped a scratch declaration that does not parse");
}

/// Every `scratch` declaration in the workspace, resolved from the graph.
///
/// **Not from the local store.** A machine that has never built has no slots, and
/// warming exactly that machine is what `pull` exists for — reading the store
/// would make the command useless in its only real use case. Costs a spec
/// resolution per target, which is why this is a rare explicit command and not
/// something a build does.
///
/// Keeps going past what will not resolve, into `gaps`: one broken package must
/// not make the whole workspace unwarmable, and the caller reports what was
/// skipped once it has acted on the rest. A declaration that does not parse is
/// handed to `report_skip`.
///
/// `rs` decides fail-fast for anything the walk builds, which says nothing
/// about whether this walk may come up short: `discovery` decides that, from
/// `--fail-fast`.
async fn declared_scratches(
    engine: &Arc<Engine>,
    rs: Arc<RequestState>,
    discovery: crate::engine::Discovery,
    report_skip: impl Fn(&Addr, &anyhow::Error),
) -> anyhow::Result<Vec<(Addr, hbuiltins::pluginscratch::ScratchDef)>> {
    use futures::TryStreamExt as _;

    // Every package: the workspace-wide selector `clean` and the gitignore walk
    // already use.
    let matcher = crate::htmatcher::Matcher::PackagePrefix(crate::htpkg::PkgBuf::from(""));
    let mut stream = Box::pin(Arc::clone(engine).query_spec(rs, &matcher, discovery));

    let mut out = Vec::new();
    while let Some(spec) = stream.try_next().await? {
        if spec.driver != hbuiltins::pluginscratch::DRIVER_NAME {
            continue;
        }
        match hbuiltins::pluginscratch::parse_declaration(&spec) {
            Ok(def) => out.push((spec.addr.clone(), def)),
            Err(err) => report_skip(&spec.addr, &err),
        }
    }
    Ok(out)
}

/// The caller has checked that exactly one of `addr` and `--all` was given.
async fn pull(
    addr: Option<String>,
    dashboard: bool,
    out: output::JsonArgs<PullRow<'static>>,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, shutdown) = bootstrap::new_engine(&global, session)?;
    let label = match &addr {
        Some(a) => format!("Fetching scratch {a}"),
        None => "Fetching scratch caches".to_string(),
    };
    run_with_progress_out(
        label,
        sink,
        session,
        global.no_tui,
        &out.clone(),
        dashboard,
        shutdown,
        move |ctx| async move {
            let rs = request_state(&engine, &ctx);
            let text = BufferedStdout::new(&ctx);
            let list = out
                .enabled()
                .then(|| out.printer().start_list())
                .transpose()?;
            let res =
                pull_selected(&engine, rs, addr.as_deref(), global.fail_fast, &text, list).await;
            text.close().await;
            res
        },
    )
    .await
}

/// `heph tool scratch pull --json`: one element per selected cache, as each
/// is fetched; `[]` when `--all` finds nothing declared `remote = True`. Left
/// without its `]` when the walk skipped part of the workspace (the command
/// then exits non-zero).
#[derive(Debug, serde::Serialize, output::Record)]
pub struct PullRow<'a> {
    /// The `scratch` target.
    addr: &'a Addr,
    /// `fetched`, `nothing_published` (no candidate lineage has a head) or
    /// `failed`.
    outcome: PullOutcome,
    /// The generation fetched, or that failed to; `null` when nothing is
    /// published.
    generation: Option<u64>,
    /// The lineage it came from (the branch's, or a fallback); `null` when
    /// nothing is published.
    lineage: Option<&'a str>,
    /// Bytes fetched; `null` unless fetched.
    bytes: Option<u64>,
    /// Why the fetch failed; `null` unless it did.
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullOutcome {
    Fetched,
    NothingPublished,
    Failed,
}

/// What fetching one cache did.
enum Pulled {
    NothingPublished,
    Fetched {
        head: RemoteHead,
        bytes: u64,
    },
    Failed {
        head: RemoteHead,
        err: anyhow::Error,
    },
}

impl Pulled {
    fn line(&self, addr: &Addr) -> String {
        match self {
            Pulled::NothingPublished => format!("{addr}: nothing published for this branch"),
            Pulled::Fetched { head, bytes } => format!(
                "{addr}: fetched generation {} from {} ({bytes} bytes)",
                head.meta.generation,
                describe_scope(&head.meta.scope),
            ),
            Pulled::Failed { err, .. } => format!("{addr}: FAILED — {err:#}"),
        }
    }

    fn row<'a>(&'a self, addr: &'a Addr) -> PullRow<'a> {
        let (outcome, head, bytes, error) = match self {
            Pulled::NothingPublished => (PullOutcome::NothingPublished, None, None, None),
            Pulled::Fetched { head, bytes } => {
                (PullOutcome::Fetched, Some(head), Some(*bytes), None)
            }
            Pulled::Failed { head, err } => (
                PullOutcome::Failed,
                Some(head),
                None,
                Some(format!("{err:#}")),
            ),
        };
        PullRow {
            addr,
            outcome,
            generation: head.map(|h| h.meta.generation),
            lineage: head.map(|h| h.meta.scope.as_str()),
            bytes,
            error,
        }
    }
}

/// Fetch what `addr` (or `--all`) selects. `list` is the `--json` document,
/// `None` for the human lines on `text`.
async fn pull_selected<W: std::io::Write>(
    engine: &Arc<Engine>,
    rs: Arc<RequestState>,
    addr: Option<&str>,
    fail_fast: bool,
    text: &BufferedStdout,
    mut list: Option<output::ListWriter<'_, PullRow<'static>, W>>,
) -> anyhow::Result<()> {
    let gaps = crate::engine::Gaps::new("//...");
    let discovery = crate::engine::Discovery::keep_going_unless(fail_fast, &gaps);
    let cancelled = {
        let rs = Arc::clone(&rs);
        move || rs.ctoken().is_cancelled()
    };
    let json = list.is_some();

    let selected: Vec<_> = declared_scratches(engine, rs, discovery, |a, err| {
        if json {
            warn_skip(a, err)
        } else {
            text.println(skip_line(a, err))
        }
    })
    .await?
    .into_iter()
    .filter(|(a, def)| match addr {
        // Naming one is an explicit instruction; honour it whatever the
        // declaration says about travelling.
        Some(want) => a.format() == want,
        // `--all` means every cache that opted in, not every cache: a
        // local-only one has nowhere to fetch from.
        None => def.remote,
    })
    .collect();

    if selected.is_empty() {
        // Nothing found may only mean it was behind what could not resolve.
        crate::commands::errors::require_complete_selection(&gaps)?;
        if let Some(want) = addr {
            anyhow::bail!(
                "no `scratch` target named {want}. `heph query -e '//...'` lists what the \
                 workspace declares"
            );
        }
        match list {
            Some(list) => list.finish(&cancelled)?,
            None => outln!(
                text,
                "No scratch cache declares `remote = True`; nothing to fetch."
            ),
        }
        return Ok(());
    }

    let scope = engine.scratch_scope().to_string();
    let fallbacks = engine.scratch_restore_scopes().to_vec();
    for (addr, def) in &selected {
        let slot = crate::engine::scratch::ResolvedScratch {
            addr: addr.clone(),
            def: def.clone(),
        }
        .slot();
        let pulled = match engine.scratch_remote_head(&slot, &scope, &fallbacks).await {
            None => Pulled::NothingPublished,
            Some(head) => {
                let dir = head_dir(&engine.home, &slot, &scope);
                match engine.scratch_pull(&head, &dir).await {
                    Ok(bytes) => {
                        crate::engine::scratch_remote::write_local_meta(
                            &engine.home,
                            &slot,
                            &scope,
                            &head.meta,
                        );
                        // Record what the slot came from, exactly as a build
                        // would. A machine warmed purely by `pull` would
                        // otherwise hold a cache that `ls` cannot name and `rm
                        // <addr>` cannot find — the store stops describing
                        // itself the moment it is populated any way but by
                        // building.
                        crate::engine::scratch_store::write_slot_meta(
                            &engine.home,
                            &slot,
                            &crate::engine::scratch_store::SlotMeta {
                                format: 1,
                                addr: addr.format(),
                                path: def.path.clone(),
                                env: def.env.clone(),
                                access: def.access.as_str().to_string(),
                                version: def.version.clone(),
                                remote: def.remote,
                            },
                        );
                        Pulled::Fetched { head, bytes }
                    }
                    Err(err) => Pulled::Failed { head, err },
                }
            }
        };
        match &mut list {
            Some(list) => {
                // A closed stdout stops the fetching: nobody reads on.
                if list.push(&pulled.row(addr))? == output::Flow::Stop
                    || list.flush()? == output::Flow::Stop
                {
                    return Ok(());
                }
            }
            None => text.println(pulled.line(addr)),
        }
    }
    // Closed only when the walk is whole (D10).
    if let Some(list) = list.filter(|_| gaps.is_empty()) {
        list.finish(&cancelled)?;
    }
    // What resolved was fetched; what did not is reported, and fails the run.
    crate::commands::errors::require_complete_selection(&gaps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::progress_app::run_with_progress;
    use crate::engine::scratch_remote::SnapshotMeta;
    use crate::engine::scratch_store::SlotMeta;

    #[test]
    fn a_lineage_is_named_for_a_human() {
        assert_eq!(describe_scope("main"), "`main`");
        // The default lineage has an empty name; printing `` for it reads like
        // something went wrong.
        assert_eq!(describe_scope(""), "the default lineage");
    }

    fn entry(meta: Option<SlotMeta>) -> SlotEntry {
        SlotEntry {
            slot: "abc123".to_string(),
            meta,
            scopes: vec!["_".to_string()],
            bytes: 0,
            last_used: None,
        }
    }

    fn meta() -> SlotMeta {
        SlotMeta {
            format: 1,
            addr: "//build:gocache".to_string(),
            path: ".cache/go-build".to_string(),
            env: "GOCACHE".to_string(),
            access: "shared".to_string(),
            version: String::new(),
            remote: false,
        }
    }

    #[test]
    fn a_slot_is_described_by_its_declaration() {
        let (name, detail) = describe(&entry(Some(meta())));
        assert_eq!(name, "//build:gocache");
        assert!(detail.contains("shared"), "{detail}");
        assert!(detail.contains(".cache/go-build"), "{detail}");
        assert!(!detail.contains("os_arch"), "{detail}");
        assert!(!detail.contains("remote"), "{detail}");
    }

    /// A mountless cache is the common shape, and `exclusive @ ` with nothing
    /// after it reads like a line that got cut off. It names the variable
    /// instead — which is how a consumer finds it, and the only locating
    /// information there is.
    #[test]
    fn a_mountless_cache_names_its_variable_instead_of_an_empty_path() {
        let mut m = meta();
        m.path = String::new();
        let (_, detail) = describe(&entry(Some(m)));
        assert!(detail.contains("GOCACHE"), "{detail}");
        assert!(
            !detail.contains(" @ "),
            "no dangling mount marker: {detail}"
        );
    }

    #[test]
    fn non_default_settings_are_called_out() {
        let mut m = meta();
        m.version = "go1.23".to_string();
        m.remote = true;
        let (_, detail) = describe(&entry(Some(m)));
        assert!(detail.contains("v=go1.23"), "{detail}");
        assert!(detail.contains("remote"), "{detail}");
    }

    /// The local walk must stop where resolution stops. Reporting a scope past
    /// the winner would say a build consults something it never reaches.
    #[test]
    fn the_local_walk_stops_at_the_first_warm_lineage() {
        let home = tempfile::tempdir().expect("tempdir");
        let warm = |sc: &str| {
            let d = head_dir(home.path(), "s1", sc);
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join("f"), b"x").expect("write");
        };
        let fbs = ["release".to_string(), "master".to_string()];

        // Nothing anywhere: every candidate is reported, all cold.
        assert_eq!(
            local_trace(home.path(), "s1", "pr-1", &fbs, true),
            [
                ("pr-1".to_string(), false),
                ("release".to_string(), false),
                ("master".to_string(), false)
            ]
        );

        // `master` warm: `release` is still consulted (it is ahead in order),
        // `master` wins, and the walk ends there.
        warm("master");
        assert_eq!(
            local_trace(home.path(), "s1", "pr-1", &fbs, true),
            [
                ("pr-1".to_string(), false),
                ("release".to_string(), false),
                ("master".to_string(), true)
            ]
        );

        // Own scope warm: no fallback is consulted at all.
        warm("pr-1");
        assert_eq!(
            local_trace(home.path(), "s1", "pr-1", &fbs, true),
            [("pr-1".to_string(), true)]
        );
    }

    /// Without `seedOnFork` a cold lineage stays cold — the fallbacks are not
    /// consulted, so reporting them would invent a restore that cannot happen.
    #[test]
    fn seeding_off_means_the_walk_never_leaves_its_own_lineage() {
        let home = tempfile::tempdir().expect("tempdir");
        let d = head_dir(home.path(), "s1", "master");
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::write(d.join("f"), b"x").expect("write");

        assert_eq!(
            local_trace(home.path(), "s1", "pr-1", &["master".to_string()], false),
            [("pr-1".to_string(), false)]
        );
    }

    /// A fallback list that names the current scope must not report it twice —
    /// it would read as a second, separate chance at the same directory.
    #[test]
    fn a_fallback_repeating_the_current_scope_is_not_a_second_chance() {
        let home = tempfile::tempdir().expect("tempdir");
        let trace = local_trace(
            home.path(),
            "s1",
            "master",
            &["master".to_string(), "release".to_string()],
            true,
        );
        assert_eq!(
            trace,
            [
                ("master".to_string(), false),
                ("release".to_string(), false)
            ]
        );
    }

    fn slot(addr: &str, remote: bool) -> SlotEntry {
        let mut m = meta();
        m.addr = addr.to_string();
        m.remote = remote;
        SlotEntry {
            slot: addr.replace(['/', ':'], "_"),
            meta: Some(m),
            scopes: vec!["_".to_string()],
            bytes: 0,
            last_used: None,
        }
    }

    /// `--all` means every cache that opted into travelling, not every cache: a
    /// local-only one has nowhere to go.
    #[test]
    fn select_all_takes_only_remote_slots() {
        let slots = vec![slot("//a:remote", true), slot("//a:local", false)];
        let got = select(slots, None, true).expect("select");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].meta.as_ref().expect("meta").addr, "//a:remote");
    }

    /// Naming a cache is an explicit instruction, so it is honoured even for one
    /// that has not opted into `--all`.
    #[test]
    fn select_by_addr_finds_it_regardless_of_the_remote_flag() {
        let slots = vec![slot("//a:local", false)];
        assert_eq!(
            select(slots, Some("//a:local"), false)
                .expect("select")
                .len(),
            1
        );
    }

    /// An unnameable slot cannot say whether it is `remote` either, so publishing
    /// it would be a guess.
    #[test]
    fn select_never_picks_a_slot_with_no_meta() {
        let slots = vec![entry(None)];
        assert!(select(slots.clone(), None, true).expect("all").is_empty());
        assert!(
            select(slots, Some("//whatever:x"), false)
                .expect("named")
                .is_empty()
        );
    }

    #[test]
    fn select_refuses_an_ambiguous_selection() {
        assert!(select(vec![], None, false).is_err(), "neither");
        assert!(select(vec![], Some("//a:b"), true).is_err(), "both");
    }

    /// An orphan is occupying disk. Hiding it would make `ls` disagree with `du`
    /// and leave no way to find what to remove.
    #[test]
    fn an_unreadable_slot_is_still_named_by_its_id() {
        let (name, _) = describe(&entry(None));
        assert!(name.contains("abc123"), "{name}");
        assert!(name.contains("unknown"), "{name}");
    }

    fn remote_head(scope: &str, generation: u64, producer: &str, produced_at: &str) -> RemoteHead {
        RemoteHead {
            cache: "r2".to_string(),
            stem: "stem".to_string(),
            meta: SnapshotMeta {
                format: 1,
                scope: scope.to_string(),
                generation,
                parent_scope: String::new(),
                parent_generation: 0,
                bytes: 2048,
                heph_version: String::new(),
                producer: producer.to_string(),
                content_hash: String::new(),
                produced_at: produced_at.to_string(),
            },
        }
    }

    fn trace(home: &Path, local: &[(&str, bool)], remote: RemoteTrace) -> HeadTrace {
        HeadTrace {
            addr: crate::htaddr::parse_addr("//build:gocache").expect("addr"),
            slot: "s1".to_string(),
            scope: local[0].0.to_string(),
            local: local
                .iter()
                .map(|&(sc, warm)| LocalCandidate {
                    scope: sc.to_string(),
                    warm,
                    dir: head_dir(home, "s1", sc),
                })
                .collect(),
            remote,
        }
    }

    /// The text `head` printed before it computed a [`HeadTrace`] first, line
    /// for line, in every branch of the walk.
    #[test]
    fn head_text_is_unchanged() {
        let home = Path::new("/h");
        let dir = |sc: &str| head_dir(home, "s1", sc).display().to_string();

        // Own lineage warm, the remote never consulted.
        let t = trace(home, &[("pr-1", true)], RemoteTrace::DeclaredLocal);
        assert_eq!(
            t.render(home),
            [
                "//build:gocache  (slot s1)".to_string(),
                format!("  * local `pr-1`: warm — {}", dir("pr-1")),
                "  (warm locally — a build here does not consult the remote)".to_string(),
                "  remote: not consulted (this cache declares `remote = False`)".to_string(),
            ]
        );

        // Seeded from a warm fallback; no remote configured.
        let t = trace(
            home,
            &[("pr-1", false), ("", true)],
            RemoteTrace::NoRemoteCache,
        );
        assert_eq!(
            t.render(home),
            [
                "//build:gocache  (slot s1)".to_string(),
                "    local `pr-1`: cold".to_string(),
                format!(
                    "  * local the default lineage: warm — seeds `pr-1` from {}",
                    dir("")
                ),
                "  (warm locally — a build here does not consult the remote)".to_string(),
                "  remote: no remote cache is configured".to_string(),
            ]
        );

        // Cold locally: the first published lineage wins, produced elsewhere;
        // a later one is shown without the star or the warning.
        let t = trace(
            home,
            &[("pr-1", false), ("master", false)],
            RemoteTrace::Consulted(vec![
                ("pr-1".to_string(), None),
                (
                    "master".to_string(),
                    Some(remote_head("master", 3, "run-7", "/ci/s1/master")),
                ),
                (
                    "release".to_string(),
                    Some(remote_head("release", 1, "", "/ci/s1/release")),
                ),
            ]),
        );
        assert_eq!(
            t.render(home),
            [
                "//build:gocache  (slot s1)".to_string(),
                "    local `pr-1`: cold".to_string(),
                "    local `master`: cold".to_string(),
                "    remote `pr-1`: nothing published".to_string(),
                "  * remote `master`: generation 3 (2.0 KiB unpacked, from `r2`, producer run-7)"
                    .to_string(),
                format!(
                    "      produced under /ci/s1/master, restores under {} — a cache whose \
                     contents embed absolute paths will restore but be inert",
                    dir("pr-1")
                ),
                "    remote `release`: generation 1 (2.0 KiB unpacked, from `r2`)".to_string(),
            ]
        );

        // Produced where it restores: no warning.
        let native = dir("master");
        let t = trace(
            home,
            &[("pr-1", false)],
            RemoteTrace::Consulted(vec![(
                "master".to_string(),
                Some(remote_head("master", 3, "", &native)),
            )]),
        );
        assert_eq!(t.render(home).len(), 3, "{:?}", t.render(home));

        // Warm locally: the remote is context, so nothing wins there.
        let t = trace(
            home,
            &[("pr-1", true)],
            RemoteTrace::Consulted(vec![(
                "pr-1".to_string(),
                Some(remote_head("pr-1", 0, "", "/elsewhere")),
            )]),
        );
        assert_eq!(
            t.render(home)[3..],
            ["    remote `pr-1`: generation 0 (2.0 KiB unpacked, from `r2`)".to_string()]
        );

        // Nothing anywhere: a build starts cold.
        let t = trace(
            home,
            &[("pr-1", false)],
            RemoteTrace::Consulted(vec![("pr-1".to_string(), None)]),
        );
        assert_eq!(
            t.render(home),
            [
                "//build:gocache  (slot s1)".to_string(),
                "    local `pr-1`: cold".to_string(),
                "    remote `pr-1`: nothing published".to_string(),
                "  nothing published in any candidate lineage — a build here starts cold"
                    .to_string(),
            ]
        );
    }

    fn json<T: serde::Serialize>(value: &T) -> serde_json::Value {
        serde_json::to_value(value).expect("serialize")
    }

    /// The record says what the text says: which lineage wins, and from where.
    #[test]
    fn head_record_names_what_a_build_restores_from() {
        let home = Path::new("/h");
        let cold_remote = trace(
            home,
            &[("pr-1", false)],
            RemoteTrace::Consulted(vec![
                ("pr-1".to_string(), None),
                (
                    "master".to_string(),
                    Some(remote_head("master", 3, "", "/ci")),
                ),
            ]),
        );
        let v = json(&cold_remote.view());
        assert_eq!(v["restores_from"], "remote");
        assert_eq!(v["restores_lineage"], "master");
        assert_eq!(v["remote_skipped"], serde_json::Value::Null);
        assert_eq!(v["remote"][0]["generation"], serde_json::Value::Null);
        assert_eq!(v["remote"][0]["producer"], serde_json::Value::Null);
        assert_eq!(v["remote"][1]["generation"], 3);
        assert_eq!(v["local"][0]["warm"], false);

        let warm = trace(
            home,
            &[("pr-1", false), ("", true)],
            RemoteTrace::DeclaredLocal,
        );
        let v = json(&warm.view());
        assert_eq!(v["restores_from"], "local");
        assert_eq!(v["restores_lineage"], "");
        assert_eq!(v["remote"], serde_json::Value::Null);
        assert_eq!(v["remote_skipped"], "declared_local");

        let cold = trace(home, &[("pr-1", false)], RemoteTrace::NoRemoteCache);
        let v = json(&cold.view());
        assert_eq!(v["restores_from"], serde_json::Value::Null);
        assert_eq!(v["restores_lineage"], serde_json::Value::Null);
        assert_eq!(v["remote_skipped"], "no_remote_cache");
    }

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        let home = Path::new("/h");
        let mut orphan = entry(None);
        orphan.bytes = 7;
        output::check_keys(&LsRow::new(&entry(Some(meta()))))?;
        output::check_keys(&LsRow::new(&orphan))?;
        let t = trace(
            home,
            &[("pr-1", false)],
            RemoteTrace::Consulted(vec![(
                "pr-1".to_string(),
                Some(remote_head("pr-1", 1, "p", "/x")),
            )]),
        );
        output::check_keys(&t.view())?;
        output::check_keys(&PathView {
            addr: "//a:b",
            path: PathBuf::from("/h/scratch/s1"),
        })?;
        output::check_keys(&RmView {
            removed: 1,
            bytes_removed: 2,
        })?;
        output::check_keys(
            &Pushed::Published {
                generation: 1,
                bytes: 2,
            }
            .row("//a:b"),
        )?;
        output::check_keys(&Pushed::Failed(anyhow::anyhow!("boom")).row("//a:b"))?;
        let addr = crate::htaddr::parse_addr("//a:b")?;
        output::check_keys(
            &Pulled::Fetched {
                head: remote_head("", 1, "", "/x"),
                bytes: 3,
            }
            .row(&addr),
        )?;
        output::check_keys(&Pulled::NothingPublished.row(&addr))
    }

    /// The rows carry what the human line says, and nothing the outcome did
    /// not produce.
    #[test]
    fn push_and_pull_rows_follow_their_outcome() {
        let v = json(&Pushed::Unchanged(4).row("//a:b"));
        assert_eq!(v["outcome"], "unchanged");
        assert_eq!(v["generation"], 4);
        assert_eq!(v["bytes"], serde_json::Value::Null);
        let v = json(&Pushed::Skipped.row("//a:b"));
        assert_eq!(v["outcome"], "skipped");
        assert_eq!(v["generation"], serde_json::Value::Null);
        let v = json(&Pushed::Failed(anyhow::anyhow!("inner").context("outer")).row("//a:b"));
        assert_eq!(v["outcome"], "failed");
        assert_eq!(v["error"], "outer: inner");

        let addr = crate::htaddr::parse_addr("//a:b").expect("addr");
        let v = json(
            &Pulled::Failed {
                head: remote_head("master", 2, "", "/x"),
                err: anyhow::anyhow!("gone"),
            }
            .row(&addr),
        );
        assert_eq!(v["outcome"], "failed");
        assert_eq!(v["generation"], 2);
        assert_eq!(v["lineage"], "master");
        assert_eq!(v["bytes"], serde_json::Value::Null);
        assert_eq!(v["error"], "gone");
        let v = json(&Pulled::NothingPublished.row(&addr));
        assert_eq!(v["outcome"], "nothing_published");
        assert_eq!(v["lineage"], serde_json::Value::Null);
    }

    fn one_scratch_engine(root: &Path) -> anyhow::Result<Arc<Engine>> {
        let mut engine = Engine::new(crate::engine::Config {
            parallelism: None,
            ..crate::engine::Config::for_tests(root)
        })?;
        let provider = hbuiltins::pluginstatictarget::Provider::new(vec![
            hbuiltins::pluginstatictarget::Target {
                addr: "//cache:go".to_string(),
                driver: hbuiltins::pluginscratch::DRIVER_NAME.to_string(),
                ..Default::default()
            },
        ])?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    /// `head` and `pull` under the line backend an agent gets: the exit is the
    /// one they had before they moved behind the progress app.
    #[tokio::test]
    async fn head_and_pull_keep_their_exits_under_the_progress_app() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let engine = one_scratch_engine(root.path())?;
        let session = Arc::new(SessionSlot::new());
        let sink = || LogSink::new_direct_with(Arc::clone(&session));
        let head = |addr: &'static str| {
            let engine = Arc::clone(&engine);
            let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
            run_with_progress(
                "t",
                sink(),
                &session,
                true,
                false,
                shutdown,
                move |ctx| async move {
                    let rs = request_state(&engine, &ctx);
                    head_trace(&engine, rs, addr, false, |_, _| {}).await?;
                    Ok(())
                },
            )
        };
        // A local-only cache: traced, and the remote is not consulted.
        head("//cache:go").await?;
        let err = head("//cache:nope").await.expect_err("no such scratch");
        assert!(
            err.to_string().contains("no `scratch` target named"),
            "{err:#}"
        );

        let pull = |addr: Option<&'static str>| {
            let engine = Arc::clone(&engine);
            let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
            run_with_progress(
                "t",
                sink(),
                &session,
                true,
                false,
                shutdown,
                move |ctx| async move {
                    let rs = request_state(&engine, &ctx);
                    let out = BufferedStdout::new(&ctx);
                    let res = pull_selected::<Vec<u8>>(&engine, rs, addr, false, &out, None).await;
                    out.close().await;
                    res
                },
            )
        };
        // `--all` with nothing declared `remote`: nothing to fetch is not a
        // failure.
        pull(None).await?;
        let err = pull(Some("//cache:nope"))
            .await
            .expect_err("no such scratch");
        assert!(
            err.to_string().contains("no `scratch` target named"),
            "{err:#}"
        );
        Ok(())
    }

    /// Under `--json`, `head` traces a local-only cache into a record that says
    /// the remote was skipped, and `pull --all` with nothing declared `remote`
    /// is a whole, empty list.
    #[tokio::test]
    async fn head_and_pull_json_documents() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let engine = one_scratch_engine(root.path())?;
        let session = Arc::new(SessionSlot::new());
        let sink = LogSink::new_direct_with(Arc::clone(&session));
        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        let doc = Arc::new(std::sync::Mutex::new((serde_json::Value::Null, Vec::new())));
        let body_doc = Arc::clone(&doc);
        let body_engine = Arc::clone(&engine);
        run_with_progress(
            "t",
            sink,
            &session,
            true,
            false,
            shutdown,
            move |ctx| async move {
                let engine = body_engine;
                let rs = request_state(&engine, &ctx);
                let trace = head_trace(&engine, rs.clone(), "//cache:go", false, warn_skip).await?;
                let head = serde_json::to_value(trace.view())?;

                let args = output::JsonArgs::<PullRow<'static>>::try_parse_from(["--json"])?;
                let mut buf = Vec::new();
                let text = BufferedStdout::new(&ctx);
                let list = args
                    .printer_to(&mut buf, output::Style::Compact)
                    .start_list()?;
                let res = pull_selected(&engine, rs, None, false, &text, Some(list)).await;
                text.close().await;
                res?;
                *body_doc.lock().expect("lock") = (head, buf);
                Ok(())
            },
        )
        .await?;

        let (head, pulled) = std::mem::take(&mut *doc.lock().expect("lock"));
        assert_eq!(head["addr"], "//cache:go");
        assert_eq!(head["remote_skipped"], "declared_local");
        assert_eq!(head["restores_from"], serde_json::Value::Null);
        assert_eq!(head["local"].as_array().map(Vec::len), Some(1));
        let pulled: serde_json::Value = serde_json::from_slice(&pulled)?;
        assert_eq!(pulled, serde_json::json!([]));
        Ok(())
    }

    /// `declared_scratches` walks on `Engine::new_state()`, whose request is
    /// fail-fast. Keep-going is decided at the call site, not read from that
    /// flag, so one broken package still leaves the rest of the workspace
    /// warmable — and is reported rather than silently dropped.
    #[tokio::test]
    async fn scratch_keeps_going_under_the_default_request_state() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = crate::engine::Engine::new(crate::engine::Config {
            parallelism: None,
            ..crate::engine::Config::for_tests(root.path())
        })?;
        let provider = hbuiltins::pluginstatictarget::Provider::new(vec![
            hbuiltins::pluginstatictarget::Target {
                addr: "//cache:go".to_string(),
                driver: hbuiltins::pluginscratch::DRIVER_NAME.to_string(),
                ..Default::default()
            },
        ])?;
        engine.register_provider(move |_| Box::new(provider))?;
        engine.register_provider(|_| {
            Box::new(
                crate::engine::fault_provider::FaultProvider::unresolvable(vec![
                    crate::htaddr::parse_addr("//broken:x").expect("addr"),
                ])
                .expect("fault provider"),
            )
        })?;
        let engine = std::sync::Arc::new(engine);
        assert!(engine.new_state().fail_fast());

        let gaps = crate::engine::Gaps::new("//...");
        let found = declared_scratches(
            &engine,
            engine.new_state(),
            crate::engine::Discovery::keep_going_unless(false, &gaps),
            |_, _| {},
        )
        .await?;
        let addrs: Vec<String> = found.iter().map(|(a, _)| a.format()).collect();
        assert_eq!(addrs, ["//cache:go"]);
        let report = gaps.report();
        assert_eq!(report.skipped, 1, "{report:?}");
        assert!(
            crate::commands::errors::require_complete_selection(&gaps).is_err(),
            "the command exits non-zero on what it skipped"
        );
        Ok(())
    }
}
