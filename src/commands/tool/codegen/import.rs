//! `heph tool codegen import`: record the files an older heph stamped with
//! its `user.heph.codegen` xattr (spec: Upgrade from the xattr). The only way
//! the import runs. Removed in v1.1.0.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::{JSON_SCHEMA, Report, blocking, run_cli};
use crate::commands::GlobalOptions;
use crate::engine::Engine;
use crate::engine::event::BuildEventKind;
use crate::engine::request_state::RequestState;
use crate::tui::LogSink;
use hwalk::codegen_index::import::{ImportCounts, scan};

#[derive(clap::Args, Clone, Debug, Default)]
pub struct Args {
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub recorded: usize,
    pub skipped: usize,
    pub unchanged: usize,
    /// `false` when the filesystem has no xattrs: nothing was read.
    pub supported: bool,
}

impl Report for ImportReport {
    fn text(&self) -> Vec<String> {
        if !self.supported {
            return vec![
                "this filesystem has no extended attributes; nothing to import".to_string(),
            ];
        }
        vec![format!(
            "recorded {} generated file{} from the previous heph's xattrs; {} skipped, {} unchanged",
            self.recorded,
            if self.recorded == 1 { "" } else { "s" },
            self.skipped,
            self.unchanged
        )]
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "schema": JSON_SCHEMA,
            "recorded": self.recorded,
            "skipped": self.skipped,
            "unchanged": self.unchanged,
        })
    }

    fn exit_code(&self) -> u8 {
        0
    }
}

/// How long the import waits on another heph's import before saying so.
const WAIT_NOTICE_AFTER: Duration = Duration::from_secs(2);

/// Import. Takes the import lock on the async side with no fixed timeout
/// (saying so after 2 s), moves it into the blocking job that scans and,
/// with no db, creates it complete; with a db, merges one target at a time,
/// each under its target lock, the import lock held throughout (J9).
pub async fn import(engine: &Arc<Engine>, rs: &Arc<RequestState>) -> anyhow::Result<ImportReport> {
    engine.open_codegen_index(rs).await?;
    let acquire = engine.codegen_locks().import(rs.ctoken());
    tokio::pin!(acquire);
    let guard = tokio::select! {
        g = &mut acquire => g?,
        () = tokio::time::sleep(WAIT_NOTICE_AFTER) => {
            tracing::warn!("waiting for another heph to import the codegen index");
            acquire.await?
        }
    };

    let index = Arc::clone(engine.codegen_index());
    let (dirs, globs) = (engine.skip_dirs(), engine.skip_globs());
    let probe = Arc::clone(engine.xattr_probe());
    let (guard, found, created) = blocking({
        let index = Arc::clone(&index);
        move || {
            let ignore = hwalk::Ignore::new(&dirs, &globs)?;
            let snap = index.snapshot()?;
            let found = scan(
                index.root(),
                &ignore,
                index.fs().as_ref(),
                probe.as_ref(),
                snap.section(),
            )?;
            let created = index.import_create(&guard, &found)?;
            Ok((guard, found, created))
        }
    })
    .await?;

    let (skipped, supported) = (found.skipped.len(), found.supported);
    let counts = match created {
        Some(c) => c,
        None => {
            let mut total = ImportCounts::default();
            for (addr, files) in found.by_addr {
                let lock = engine.codegen_locks().target(&addr, rs.ctoken()).await?;
                let index = Arc::clone(&index);
                total.add(blocking(move || index.import_merge(&lock, &files)).await?);
            }
            total
        }
    };
    drop(guard);
    finish(rs, counts, skipped, supported)
}

fn finish(
    rs: &Arc<RequestState>,
    counts: ImportCounts,
    skipped: usize,
    supported: bool,
) -> anyhow::Result<ImportReport> {
    if skipped > 0 {
        tracing::warn!(
            skipped,
            "{skipped} file{} carry an unreadable codegen xattr (empty, not UTF-8, or not a target); not recorded",
            if skipped == 1 { "" } else { "s" }
        );
    }
    rs.emit(BuildEventKind::CodegenImported {
        files: counts.recorded,
        skipped,
    });
    Ok(ImportReport {
        recorded: counts.recorded,
        skipped,
        unchanged: counts.unchanged,
        supported,
    })
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    run_cli(
        "Importing codegen xattrs",
        args.json,
        sink,
        global,
        |engine, rs| async move { import(&engine, &rs).await },
    )
}
