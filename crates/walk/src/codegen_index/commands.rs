//! What the `heph tool codegen` commands change in the index. Each runs on the
//! blocking pool under one target's codegen lock, which the engine took on the
//! async side (J9): file IO first, then one transaction that only writes rows,
//! published before it returns.

use super::TargetLockGuard;
use super::fs::lstat_opt;
use super::index::CodegenIndex;
use super::paths::rel_path;
use super::store::{Row, RowState};
use super::write::fresh_row_hash;
use anyhow::{Context as _, Result};

/// What `discard` did, or would do, with one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardOutcome {
    /// The file is deleted and its row dropped.
    Discarded,
    /// The file was already gone; its row is dropped.
    Gone,
    /// `--dry-run`: the file would be deleted (`gone`: it already is) and
    /// its row dropped.
    WouldDiscard { gone: bool },
    /// Nothing changed, for this reason.
    Refused(String),
}

/// One path's `discard` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discarded {
    pub path: Vec<u8>,
    pub outcome: DiscardOutcome,
}

/// Whether the row's bytes are only known from an import: its record is what
/// the import found, not what heph wrote, so a delete needs `--force` (J5).
/// A `dropped` row keeps the `output` its write-back set; an import sets none.
pub fn imported_provenance(row: &Row) -> bool {
    row.state == RowState::Imported || row.output.is_none()
}

impl CodegenIndex {
    /// Set `seen_in_section` on the target's rows at `paths`, which the
    /// section just written covers. Only ever set (J1). Returns how many
    /// rows changed.
    pub fn mark_seen_in_section(
        &self,
        guard: &TargetLockGuard,
        paths: &[Vec<u8>],
    ) -> Result<usize> {
        let Some(store) = self.open_store()? else {
            return Ok(0);
        };
        let addr = guard.addr();
        // Read first: a call that changes nothing commits nothing, so the
        // generation (and every process's snapshot) stays put.
        let mut todo = Vec::new();
        for path in paths {
            if store
                .row(path)?
                .is_some_and(|r| r.addr == addr && !r.seen_in_section)
            {
                todo.push(path);
            }
        }
        if todo.is_empty() {
            return Ok(0);
        }
        let committed = store.write(|txn| {
            let mut n = 0;
            for path in todo {
                let Some(mut row) = txn.row(path)?.filter(|r| r.addr == addr) else {
                    continue;
                };
                if row.seen_in_section {
                    continue;
                }
                row.seen_in_section = true;
                txn.put(&row)?;
                n += 1;
            }
            Ok(n)
        })?;
        self.publish_committed(&committed);
        Ok(committed.value)
    }

    /// `codegen keep`: drop the target's rows at `paths`; the files stay.
    /// Returns the paths whose row was dropped.
    pub fn forget(&self, guard: &TargetLockGuard, paths: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        let Some(store) = self.open_store()? else {
            return Ok(Vec::new());
        };
        let addr = guard.addr();
        // Read first, as `mark_seen_in_section` does: no row, no commit.
        let mut todo = Vec::new();
        for path in paths {
            if store.row(path)?.is_some_and(|r| r.addr == addr) {
                todo.push(path);
            }
        }
        if todo.is_empty() {
            return Ok(Vec::new());
        }
        let committed = store.write(|txn| {
            let mut dropped = Vec::new();
            for path in todo {
                if txn.delete(path, addr)? {
                    dropped.push(path.clone());
                }
            }
            Ok(dropped)
        })?;
        self.publish_committed(&committed);
        Ok(committed.value)
    }

    /// `codegen discard`: delete each file at `paths` that the target's row
    /// records, after a fresh, uncached hash equals the record (J5), and drop
    /// its row. A file whose bytes differ, or whose row came from an import,
    /// is refused unless `force`. A pending row is always refused. `dry_run`
    /// changes nothing and reports what would happen.
    pub fn discard(
        &self,
        guard: &TargetLockGuard,
        paths: &[Vec<u8>],
        force: bool,
        dry_run: bool,
    ) -> Result<Vec<Discarded>> {
        let Some(store) = self.open_store()? else {
            return Ok(paths
                .iter()
                .map(|p| Discarded {
                    path: p.clone(),
                    outcome: DiscardOutcome::Refused("heph never wrote it".into()),
                })
                .collect());
        };
        let addr = guard.addr();
        let fs = self.fs().as_ref();
        let mut out = Vec::with_capacity(paths.len());
        let mut drop_rows = Vec::new();
        for path in paths {
            let refuse = |why: String| Discarded {
                path: path.clone(),
                outcome: DiscardOutcome::Refused(why),
            };
            let Some(row) = store.row(path)?.filter(|r| r.addr == addr) else {
                out.push(refuse("heph never wrote it".into()));
                continue;
            };
            if row.pending_hash.is_some() {
                out.push(refuse(format!(
                    "a write-back of {addr} was interrupted; rebuild {addr} first"
                )));
                continue;
            }
            let abs = self.root().join(rel_path(path));
            let st = lstat_opt(fs, &abs)?;
            let Some(st) = st else {
                out.push(Discarded {
                    path: path.clone(),
                    outcome: if dry_run {
                        DiscardOutcome::WouldDiscard { gone: true }
                    } else {
                        DiscardOutcome::Gone
                    },
                });
                drop_rows.push(path.clone());
                continue;
            };
            if !force {
                if imported_provenance(&row) {
                    out.push(refuse(
                        "recorded from an older heph's xattr, not written by this one; \
                         --force deletes it"
                            .into(),
                    ));
                    continue;
                }
                let fresh = fresh_row_hash(fs, &abs, row.kind, Some(&st))?;
                let same = matches!(
                    (&fresh, &row.record),
                    (Some((h, _)), Some(rec)) if *h == rec.hash
                );
                if !same {
                    out.push(refuse(
                        "modified since heph wrote it; --force deletes it".into(),
                    ));
                    continue;
                }
            }
            if dry_run {
                out.push(Discarded {
                    path: path.clone(),
                    outcome: DiscardOutcome::WouldDiscard { gone: false },
                });
                continue;
            }
            match fs.unlink(&abs) {
                Ok(()) => {}
                Err(e) if super::fs::is_absent(&e) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("deleting {}", abs.display()));
                }
            }
            out.push(Discarded {
                path: path.clone(),
                outcome: DiscardOutcome::Discarded,
            });
            drop_rows.push(path.clone());
        }
        if dry_run || drop_rows.is_empty() {
            return Ok(out);
        }
        let committed = store.write(|txn| {
            for path in &drop_rows {
                txn.delete(path, addr)?;
            }
            Ok(())
        })?;
        self.publish_committed(&committed);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::codegen_index::store::{Row, RowKind};
    use crate::codegen_index::testfs::{guard, ws};

    const ADDR: &str = "//p:gen";

    fn row(path: &str, addr: &str, seen: bool) -> Row {
        let mut r = Row::new(path.as_bytes().to_vec(), addr, RowKind::File);
        r.seen_in_section = seen;
        r
    }

    #[test]
    fn mark_seen_in_section_that_changes_nothing_commits_nothing() {
        let w = ws();
        w.put(&[
            row("gen/a.go", ADDR, true),
            row("gen/b.go", "//p:other", false),
        ]);
        let g = w.generation();
        let paths = [
            b"gen/a.go".to_vec(),
            b"gen/b.go".to_vec(),
            b"gen/none".to_vec(),
        ];
        let n = w.index.mark_seen_in_section(&guard(ADDR), &paths).unwrap();
        assert_eq!(n, 0);
        assert_eq!(w.generation(), g, "a no-op moved the generation");
        assert!(!w.db_row("gen/b.go").unwrap().seen_in_section);
        // One that changes something commits once.
        w.put(&[row("gen/c.go", ADDR, false)]);
        let g = w.generation();
        let n = w
            .index
            .mark_seen_in_section(&guard(ADDR), &[b"gen/c.go".to_vec()])
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(w.generation(), g + 1);
        assert!(w.db_row("gen/c.go").unwrap().seen_in_section);
    }

    #[test]
    fn forget_that_changes_nothing_commits_nothing() {
        let w = ws();
        w.put(&[row("gen/b.go", "//p:other", false)]);
        let g = w.generation();
        let paths = [b"gen/b.go".to_vec(), b"gen/none".to_vec()];
        assert!(w.index.forget(&guard(ADDR), &paths).unwrap().is_empty());
        assert_eq!(w.generation(), g, "a no-op moved the generation");
        assert!(w.db_row("gen/b.go").is_some(), "another target's row");
        w.put(&[row("gen/a.go", ADDR, false)]);
        let g = w.generation();
        let dropped = w
            .index
            .forget(&guard(ADDR), &[b"gen/a.go".to_vec()])
            .unwrap();
        assert_eq!(dropped, [b"gen/a.go".to_vec()]);
        assert_eq!(w.generation(), g + 1);
        assert!(w.db_row("gen/a.go").is_none());
    }
}
