//! `heph tool scratch …` — inspect and reclaim persistent cache directories.
//!
//! Sits in `heph tool` alongside `cache`, `gc` and `clean`, which is the group for
//! commands that inspect or repair heph's own state rather than build anything.
//! A scratch slot is exactly that kind of state.
//!
//! Everything here reads the store directly and resolves no BUILD files, so it
//! keeps working when the targets that produced a slot have been deleted or
//! renamed — the same property `heph tool clean` has for addr-only selections.

use crate::commands::bootstrap;
use crate::engine::scratch_store::{SlotEntry, store_root};
use crate::tui::LogSink;

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
    Ls,
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
    },
    /// Print the on-disk path of a scratch cache
    ///
    /// For pointing another tool at it, or `du`-ing it by hand.
    ///
    /// Example: `heph tool scratch path //build:gocache`
    Path {
        /// Address of the `scratch` target, e.g. `//build:gocache`.
        addr: String,
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
    },
}

impl ScratchArgs {
    pub fn execute(&self, _sink: LogSink) -> anyhow::Result<()> {
        match &self.command {
            ScratchCommands::Ls => bootstrap::block_on(ls())?,
            ScratchCommands::Head { addr } => bootstrap::block_on(head(addr))?,
            ScratchCommands::Path { addr } => bootstrap::block_on(path(addr))?,
            ScratchCommands::Rm { addr, all } => {
                bootstrap::block_on(rm(addr.as_deref(), *all))?
            }
            ScratchCommands::Push {
                addr,
                all,
                force,
                producer,
            } => bootstrap::block_on(push(addr.as_deref(), *all, *force, producer.clone()))?,
            ScratchCommands::Pull { addr, all } => {
                bootstrap::block_on(pull(addr.as_deref(), *all))?
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

async fn ls() -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let mut slots = engine.scratch_slots()?;
    if slots.is_empty() {
        println!("No scratch caches in this workspace.");
        return Ok(());
    }
    // Largest first: the reason to run this is usually disk.
    slots.sort_by_key(|s| std::cmp::Reverse(s.bytes));

    let total: u64 = slots.iter().map(|s| s.bytes).sum();
    let width = slots
        .iter()
        .map(|s| describe(s).0.len())
        .max()
        .unwrap_or(0)
        .max(7);

    println!("{:<width$}  {:>10}  LINEAGES", "CACHE", "SIZE");
    for slot in &slots {
        let (name, detail) = describe(slot);
        let scopes = if slot.scopes.is_empty() {
            "-".to_string()
        } else {
            slot.scopes.join(", ")
        };
        println!(
            "{name:<width$}  {:>10}  {scopes}",
            hcore::units::human_bytes(slot.bytes)
        );
        if !detail.is_empty() {
            println!("{:<width$}  {:>10}  {detail}", "", "");
        }
    }
    println!();
    println!(
        "{} cache(s), {} total",
        slots.len(),
        hcore::units::human_bytes(total)
    );
    Ok(())
}

/// The local lineages a build would consult, in order, each with whether it is
/// warm. Stops at the first warm one, exactly as
/// `engine::scratch::resolve_scope_dir` does — this walk is an explanation of
/// that one, so any divergence is a lie rather than a cosmetic difference.
fn local_trace(
    home: &std::path::Path,
    slot: &str,
    scope: &str,
    fallbacks: &[String],
    seed_on_fork: bool,
) -> Vec<(String, bool)> {
    let has = |sc: &str| {
        let p = crate::engine::scratch_remote::scope_head_dir(home, slot, sc);
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

async fn head(addr: &str) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let (found_addr, def) = declared_scratches(&engine)
        .await?
        .into_iter()
        .find(|(a, _)| a.format() == addr)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no `scratch` target named {addr}. `heph query -e '//...'` lists what the \
                 workspace declares"
            )
        })?;

    let slot = crate::engine::scratch::ResolvedScratch {
        addr: found_addr.clone(),
        def: def.clone(),
    }
    .slot();
    let scope = engine.scratch_scope().to_string();
    let fallbacks = engine.scratch_restore_scopes().to_vec();

    println!("{found_addr}  (slot {slot})");

    // Local first, and with its *own* fallbacks — a build seeds a cold lineage
    // from a warm sibling before it ever looks at the remote, so a trace that
    // skipped straight to the network would name the wrong winner in exactly the
    // case this command exists for: a fresh branch on a machine that has built
    // its base.
    let local = crate::engine::scratch_remote::scope_head_dir(&engine.home, &slot, &scope);
    let head_of = |sc: &str| crate::engine::scratch_remote::scope_head_dir(&engine.home, &slot, sc);

    let trace = local_trace(
        &engine.home,
        &slot,
        &scope,
        &fallbacks,
        engine.scratch_seeds_on_fork(),
    );
    let warm = trace.last().is_some_and(|(_, w)| *w);
    for (sc, is_warm) in &trace {
        let dir = head_of(sc);
        if !is_warm {
            println!("    local {}: cold", describe_scope(sc));
        } else if *sc == scope {
            println!("  * local {}: warm — {}", describe_scope(sc), dir.display());
        } else {
            println!(
                "  * local {}: warm — seeds {} from {}",
                describe_scope(sc),
                describe_scope(&scope),
                dir.display()
            );
        }
    }
    if warm {
        // Resolution stops here; everything below is printed for context, not as
        // a prediction. Saying so beats letting the remote section imply a fetch
        // that will not happen.
        println!("  (warm locally — a build here does not consult the remote)");
    }

    if !def.remote {
        println!("  remote: not consulted (this cache declares `remote = False`)");
        return Ok(());
    }
    if engine.remote_caches().is_empty() {
        println!("  remote: no remote cache is configured");
        return Ok(());
    }

    let trace = engine.scratch_remote_trace(&slot, &scope, &fallbacks).await;
    let mut winner_shown = false;
    for (scope, head) in &trace {
        match head {
            Some(h) => {
                // The first scope with anything is the one a build takes; the
                // rest are printed so the ordering is visible rather than
                // implied.
                let wins = !winner_shown && !warm;
                winner_shown = true;
                // `bytes` is the unpacked size; `push` reports what it
                // uploaded. Saying which this is stops the two commands from
                // looking like they disagree about one snapshot.
                let mut line = format!(
                    "  {} remote {}: generation {} ({} unpacked, from `{}`",
                    if wins { "*" } else { " " },
                    describe_scope(scope),
                    h.meta.generation,
                    hcore::units::human_bytes(h.meta.bytes),
                    h.cache,
                );
                if !h.meta.producer.is_empty() {
                    line.push_str(&format!(", producer {}", h.meta.producer));
                }
                line.push(')');
                println!("{line}");
                // Only for the entry that would actually be restored — the
                // others are never unpacked, so where they were produced says
                // nothing about this machine.
                // Compare against where *its own* lineage lives here: the tail
                // differs by scope on every cross-branch restore, which is
                // ordinary, so comparing full paths would warn constantly. A
                // difference against its own scope's path means a different
                // workspace or home — the case that actually breaks a cache
                // whose contents embed absolute paths.
                let native = head_of(&h.meta.scope);
                if wins && h.meta.produced_at != native.to_string_lossy() {
                    println!(
                        "      produced under {}, restores under {} — a cache whose \
                         contents embed absolute paths will restore but be inert",
                        h.meta.produced_at,
                        local.display()
                    );
                }
            }
            None => println!("    remote {}: nothing published", describe_scope(scope)),
        }
    }
    if !winner_shown && !warm {
        // Only cold if the local head is cold too — resolution consults the
        // remote *because* local missed, so an empty remote with a warm local is
        // a perfectly good build.
        if warm {
            println!(
                "  nothing published in any candidate lineage; the local head is warm, so a \
                 build here uses it"
            );
        } else {
            println!("  nothing published in any candidate lineage — a build here starts cold");
        }
    }
    Ok(())
}

async fn path(addr: &str) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let slots = engine.scratch_slots()?;
    let found = slots
        .iter()
        .find(|s| s.meta.as_ref().is_some_and(|m| m.addr == addr));
    match found {
        Some(slot) => {
            println!("{}", store_root(&engine.home).join(&slot.slot).display());
            Ok(())
        }
        // Not an error to be cold — a cache that has never been built has no
        // directory yet, and saying "no such thing" would be wrong.
        None => anyhow::bail!(
            "no scratch cache on disk for {addr}. It may simply never have been built; \
             `heph tool scratch ls` shows what is there"
        ),
    }
}

async fn rm(addr: Option<&str>, all: bool) -> anyhow::Result<()> {
    if all == addr.is_some() {
        anyhow::bail!("pass either an address or --all, not both or neither");
    }
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let (removed, freed) = engine.scratch_remove(addr)?;
    if removed == 0 {
        println!("Nothing to remove.");
    } else {
        println!(
            "Removed {removed} cache(s), freed {}.",
            hcore::units::human_bytes(freed)
        );
    }
    Ok(())
}

/// Pick the slots a `push`/`pull` selection names.
///
/// A slot with no readable meta is never selected: it cannot be named, and it
/// cannot say whether it is `remote` either, so publishing it would be a guess.
fn select(
    slots: Vec<crate::engine::scratch_store::SlotEntry>,
    addr: Option<&str>,
    all: bool,
) -> anyhow::Result<Vec<crate::engine::scratch_store::SlotEntry>> {
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

async fn push(addr: Option<&str>, all: bool, force: bool, producer: String) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let selected = select(engine.scratch_slots()?, addr, all)?;
    if selected.is_empty() {
        println!("Nothing to publish.");
        return Ok(());
    }

    let scope = engine.scratch_scope().to_string();
    let mut failed = 0usize;
    for slot in &selected {
        let name = slot
            .meta
            .as_ref()
            .map(|m| m.addr.clone())
            .unwrap_or_else(|| slot.slot.clone());
        let dir = crate::engine::scratch_remote::scope_head_dir(&engine.home, &slot.slot, &scope);
        if !dir.is_dir() {
            println!("{name}: nothing built in this lineage, skipped");
            continue;
        }
        let parent =
            crate::engine::scratch_remote::read_local_meta(&engine.home, &slot.slot, &scope);
        let parent = if force { None } else { parent };
        match engine
            .scratch_push(&slot.slot, &scope, &dir, parent.as_ref(), &producer)
            .await
        {
            Ok((_, 0)) => println!("{name}: unchanged, skipped"),
            Ok((generation, bytes)) => {
                println!("{name}: published generation {generation} ({bytes} bytes)")
            }
            Err(err) => {
                // Reported per slot and counted, rather than aborting: one
                // unpublishable cache should not silently drop the rest.
                println!("{name}: FAILED — {err:#}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} scratch cache(s) failed to publish");
    }
    Ok(())
}

/// Every `scratch` declaration in the workspace, resolved from the graph.
///
/// **Not from the local store.** A machine that has never built has no slots, and
/// warming exactly that machine is what `pull` exists for — reading the store
/// would make the command useless in its only real use case. Costs a spec
/// resolution per target, which is why this is a rare explicit command and not
/// something a build does.
async fn declared_scratches(
    engine: &std::sync::Arc<crate::engine::Engine>,
) -> anyhow::Result<Vec<(crate::htaddr::Addr, hbuiltins::pluginscratch::ScratchDef)>> {
    use futures::StreamExt as _;

    // Every package: the workspace-wide selector `clean` and the gitignore walk
    // already use.
    let matcher = crate::htmatcher::Matcher::PackagePrefix(crate::htpkg::PkgBuf::from(""));
    let rs = engine.new_state();
    let mut stream = Box::pin(std::sync::Arc::clone(engine).query_spec(rs, &matcher));

    let mut out = Vec::new();
    while let Some(spec) = stream.next().await {
        // A target whose spec will not resolve is not this command's problem — it
        // is reported by anything that actually builds it. Skipping keeps one
        // broken package from making the whole workspace unwarmable.
        let Ok(spec) = spec else { continue };
        if spec.driver != hbuiltins::pluginscratch::DRIVER_NAME {
            continue;
        }
        match hbuiltins::pluginscratch::parse_declaration(&spec) {
            Ok(def) => out.push((spec.addr.clone(), def)),
            Err(err) => println!("{}: skipped — {err:#}", spec.addr),
        }
    }
    Ok(out)
}

async fn pull(addr: Option<&str>, all: bool) -> anyhow::Result<()> {
    if all == addr.is_some() {
        anyhow::bail!("pass either an address or --all, not both or neither");
    }
    let (engine, _shutdown) = bootstrap::new_engine()?;

    let selected: Vec<_> = declared_scratches(&engine)
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
        match addr {
            Some(want) => anyhow::bail!(
                "no `scratch` target named {want}. `heph query -e '//...'` lists what the \
                 workspace declares"
            ),
            None => println!("No scratch cache declares `remote = True`; nothing to fetch."),
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
        let Some(head) = engine.scratch_remote_head(&slot, &scope, &fallbacks).await else {
            println!("{addr}: nothing published for this branch");
            continue;
        };
        let dir = crate::engine::scratch_remote::scope_head_dir(&engine.home, &slot, &scope);
        match engine.scratch_pull(&head, &dir).await {
            Ok(bytes) => {
                crate::engine::scratch_remote::write_local_meta(
                    &engine.home,
                    &slot,
                    &scope,
                    &head.meta,
                );
                // Record what the slot came from, exactly as a build would. A
                // machine warmed purely by `pull` would otherwise hold a cache
                // that `ls` cannot name and `rm <addr>` cannot find — the store
                // stops describing itself the moment it is populated any way but
                // by building.
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
                println!(
                    "{addr}: fetched generation {} from {} ({bytes} bytes)",
                    head.meta.generation,
                    describe_scope(&head.meta.scope),
                );
            }
            Err(err) => println!("{addr}: FAILED — {err:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
            let d = crate::engine::scratch_remote::scope_head_dir(home.path(), "s1", sc);
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
        let d = crate::engine::scratch_remote::scope_head_dir(home.path(), "s1", "master");
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

    fn slot(addr: &str, remote: bool) -> crate::engine::scratch_store::SlotEntry {
        let mut m = meta();
        m.addr = addr.to_string();
        m.remote = remote;
        crate::engine::scratch_store::SlotEntry {
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
}
