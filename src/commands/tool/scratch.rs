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
            ScratchCommands::Ls => bootstrap::block_on(ls())?,
            ScratchCommands::Path { addr } => bootstrap::block_on(path(addr))?,
            ScratchCommands::Rm { addr, all } => bootstrap::block_on(rm(addr.as_deref(), *all))?,
        }
    }
}

/// Render a byte count the way a person reads one.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut chosen: Option<&str> = None;
    for unit in UNITS {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        chosen = Some(unit);
    }
    match chosen {
        None => format!("{n} B"),
        Some(unit) => format!("{v:.1} {unit}"),
    }
}

fn describe(slot: &SlotEntry) -> (String, String) {
    match &slot.meta {
        Some(m) => {
            let mut detail = format!("{} @ {}", m.access, m.path);
            if !m.version.is_empty() {
                detail.push_str(&format!(" v={}", m.version));
            }
            if m.platform != "os_arch" {
                detail.push_str(&format!(" platform={}", m.platform));
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
        println!("{name:<width$}  {:>10}  {scopes}", human_bytes(slot.bytes));
        if !detail.is_empty() {
            println!("{:<width$}  {:>10}  {detail}", "", "");
        }
    }
    println!();
    println!("{} cache(s), {} total", slots.len(), human_bytes(total));
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
        println!("Removed {removed} cache(s), freed {}.", human_bytes(freed));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scratch_store::SlotMeta;

    #[test]
    fn human_bytes_reads_like_a_person_wrote_it() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        // Exact powers stay exact rather than becoming "1024.0 B".
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(10 * 1024 * 1024 * 1024), "10.0 GiB");
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
            platform: "os_arch".to_string(),
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
        // The default platform is not worth a column of noise.
        assert!(!detail.contains("os_arch"), "{detail}");
        assert!(!detail.contains("remote"), "{detail}");
    }

    #[test]
    fn non_default_settings_are_called_out() {
        let mut m = meta();
        m.platform = "any".to_string();
        m.version = "go1.23".to_string();
        m.remote = true;
        let (_, detail) = describe(&entry(Some(m)));
        assert!(detail.contains("platform=any"), "{detail}");
        assert!(detail.contains("v=go1.23"), "{detail}");
        assert!(detail.contains("remote"), "{detail}");
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
