use std::path::PathBuf;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;
use serde::Serialize;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::failure_slot::{FIXES, FailureSlots, LOG, OUT};
use crate::htaddr::{self, Addr};
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Emit JSON instead of one labelled path per line
    #[arg(long)]
    pub json: bool,
}

/// Where a target's last recorded failure is kept: its slot, and each part
/// of it that the failed run left. Paths are absolute; an absent part is
/// `None` (`null` in `--json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailureView {
    pub addr: String,
    pub slot: PathBuf,
    /// The input hash of the run that failed. It may no longer be the
    /// target's: nothing is resolved to compare.
    pub hashin: String,
    /// When it failed, in milliseconds since the Unix epoch.
    pub failed_at: u64,
    pub log: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub fixes: Option<PathBuf>,
}

/// The failure recorded for `addr` in `slots`, read from its slot alone:
/// nothing is built or resolved. `None` when there is no complete slot for
/// it — the target never failed, or passed or resolved from cache since.
pub fn failure_view(slots: &FailureSlots, addr: &Addr) -> anyhow::Result<Option<FailureView>> {
    let Some(manifest) = slots.read(addr) else {
        return Ok(None);
    };
    let slot = std::path::absolute(slots.slot_dir(addr))
        .context("making the failure slot's path absolute")?;
    let part = |name: &str, is: fn(&std::path::Path) -> bool| {
        let p = slot.join(name);
        is(&p).then_some(p)
    };
    Ok(Some(FailureView {
        addr: manifest.addr,
        hashin: manifest.hashin,
        failed_at: manifest.failed_at,
        log: part(LOG, std::path::Path::is_file),
        out: part(OUT, std::path::Path::is_dir),
        fixes: part(FIXES, std::path::Path::is_file),
        slot,
    }))
}

/// The text form: the slot, then each part present, one labelled path per
/// line.
fn render_text(view: &FailureView) -> String {
    let mut out = format!("slot: {}\n", view.slot.display());
    for (label, part) in [
        ("log", &view.log),
        ("out", &view.out),
        ("fixes", &view.fixes),
    ] {
        if let Some(p) = part {
            out.push_str(&format!("{label}: {}\n", p.display()));
        }
    }
    out
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, _sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let base = crate::engine::get_cwp()?;
    let addr = htaddr::parse_addr_with_base(args.addr.as_ref(), &base)
        .with_context(|| format!("parse {}", args.addr))?;
    let (engine, _shutdown) = bootstrap::new_engine(&global)?;
    let Some(view) = failure_view(engine.failure_slots(), &addr)? else {
        anyhow::bail!("no recorded failure for {}", addr.format());
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&view).context("serialize failure")?
        );
    } else {
        print!("{}", render_text(&view));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot for `addr` under a fresh home, holding a log, an `out/` and,
    /// when `fixes`, a `fixes.txt`.
    fn slot(addr: &Addr, fixes: bool) -> (tempfile::TempDir, FailureSlots) {
        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        let dir = slots.slot_dir(addr);
        std::fs::create_dir_all(dir.join(OUT)).expect("mkdir out");
        std::fs::write(dir.join(LOG), "boom\n").expect("write log");
        if fixes {
            std::fs::write(dir.join(FIXES), "web/a.ts\n").expect("write fixes");
        }
        let manifest = serde_json::json!({
            "version": 1, "addr": addr.format(), "hashin": "h1", "failed_at": 42,
        });
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).expect("write manifest");
        (home, slots)
    }

    #[test]
    fn failure_prints_slot_paths() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, false);
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        let dir = slots.slot_dir(&addr);
        assert_eq!(
            render_text(&view),
            format!(
                "slot: {}\nlog: {}\nout: {}\n",
                dir.display(),
                dir.join(LOG).display(),
                dir.join(OUT).display()
            )
        );

        // Another address has no slot: nothing to print, and no slot made.
        let other = htaddr::parse_addr("//web:other").expect("addr");
        assert_eq!(failure_view(&slots, &other).expect("read"), None);
        assert!(!slots.slot_dir(&other).exists());
    }

    #[test]
    fn failure_json_shape() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, true);
        // A slot whose run kept no `out/`: that part is `null`.
        std::fs::remove_dir(slots.slot_dir(&addr).join(OUT)).expect("rm out");
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        let dir = slots.slot_dir(&addr);
        assert!(dir.is_absolute());
        let json = serde_json::to_value(&view).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "addr": "//web:lint",
                "slot": dir,
                "hashin": "h1",
                "failed_at": 42,
                "log": dir.join(LOG),
                "out": null,
                "fixes": dir.join(FIXES),
            })
        );
    }
}
