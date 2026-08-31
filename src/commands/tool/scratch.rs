//! `heph tool scratch …` — inspect and reclaim persistent cache directories.
//!
//! Sits in `heph tool` alongside `cache`, `gc` and `clean`, which is the group for
//! commands that inspect or repair heph's own state rather than build anything.
//! A scratch slot is exactly that kind of state.
//!
//! Everything here reads the store directly and resolves no BUILD files, so it
//! keeps working when the targets that produced a slot have been deleted or
//! renamed — the same property `heph tool clean` has for addr-only selections.

use crate::commands::GlobalOptions;
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
    pub fn execute(&self, _sink: LogSink, _global: &GlobalOptions) -> anyhow::Result<()> {
        match &self.command {
            ScratchCommands::Ls => bootstrap::block_on(ls(_global))?,
            ScratchCommands::Path { addr } => bootstrap::block_on(path(addr, _global))?,
            ScratchCommands::Rm { addr, all } => {
                bootstrap::block_on(rm(addr.as_deref(), *all, _global))?
            }
            ScratchCommands::Push {
                addr,
                all,
                force,
                producer,
            } => bootstrap::block_on(push(
                addr.as_deref(),
                *all,
                *force,
                producer.clone(),
                _global,
            ))?,
            ScratchCommands::Pull { addr, all } => {
                bootstrap::block_on(pull(addr.as_deref(), *all, _global))?
            }
        }
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

async fn ls(global: &GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global)?;
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

async fn path(addr: &str, global: &GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global)?;
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

async fn rm(addr: Option<&str>, all: bool, global: &GlobalOptions) -> anyhow::Result<()> {
    if all == addr.is_some() {
        anyhow::bail!("pass either an address or --all, not both or neither");
    }
    let (engine, _shutdown) = bootstrap::new_engine(global)?;
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

async fn push(
    addr: Option<&str>,
    all: bool,
    force: bool,
    producer: String,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global)?;
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

async fn pull(addr: Option<&str>, all: bool, global: &GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global)?;
    let selected = select(engine.scratch_slots()?, addr, all)?;
    if selected.is_empty() {
        println!(
            "Nothing to fetch. A cache must have been built at least once, and declared `remote = True`."
        );
        return Ok(());
    }

    let scope = engine.scratch_scope().to_string();
    let fallbacks = engine.scratch_restore_scopes().to_vec();
    for slot in &selected {
        let name = slot
            .meta
            .as_ref()
            .map(|m| m.addr.clone())
            .unwrap_or_else(|| slot.slot.clone());
        let Some(head) = engine
            .scratch_remote_head(&slot.slot, &scope, &fallbacks)
            .await
        else {
            println!("{name}: nothing published for this branch");
            continue;
        };
        let dir = crate::engine::scratch_remote::scope_head_dir(&engine.home, &slot.slot, &scope);
        match engine.scratch_pull(&head, &dir).await {
            Ok(bytes) => {
                crate::engine::scratch_remote::write_local_meta(
                    &engine.home,
                    &slot.slot,
                    &scope,
                    &head.meta,
                );
                println!(
                    "{name}: fetched generation {} from `{}` ({bytes} bytes)",
                    head.meta.generation, head.meta.scope
                );
            }
            Err(err) => println!("{name}: FAILED — {err:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scratch_store::SlotMeta;

    #[test]
    fn human_bytes_reads_like_a_person_wrote_it() {
        assert_eq!(hcore::units::human_bytes(0), "0 B");
        assert_eq!(hcore::units::human_bytes(999), "999 B");
        // Exact powers stay exact rather than becoming "1024.0 B".
        assert_eq!(hcore::units::human_bytes(1024), "1.0 KiB");
        assert_eq!(hcore::units::human_bytes(1536), "1.5 KiB");
        assert_eq!(
            hcore::units::human_bytes(10 * 1024 * 1024 * 1024),
            "10.0 GiB"
        );
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
