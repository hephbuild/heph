//! Lookups: what one path is, given a [`Snapshot`] (spec: Mechanism, Lookup).
//!
//! A lookup only reads (J6): no defs, no engine call, no index write, no wait
//! on the index. It answers `Generated`, `Released`, `Modified` or `Source`,
//! with a reason and the section line that decided, for `why`.
//!
//! 0. Probe the path. On a miss, and only when the fold map is not empty,
//!    probe the fold multimap; a candidate is used only when the walked file
//!    and the row's recorded path lstat to the same `(dev, ino)`. After a fold
//!    hit the section is tested on the row's recorded path, as a point lookup.
//! 1. A pending row is `Generated` (`pending`), whatever the bytes.
//! 2. A verified row (stat equal outside the racy window, else the cached
//!    content hash; a symlink by its kind and `readlink`) is `Generated`, or
//!    `Released` when no line matches, the section is present and the row is
//!    `seen_in_section`.
//! 3. A row whose bytes differ is `Modified` under a line, else `Source`.
//! 4. No row: `Generated` (`unrecorded`) under a line, else `Source`.

use super::fold::fold_hash;
use super::fs::{FileStat, IndexFs, StatKind, is_absent, lstat_opt};
use super::index::SECTION_RACY_SLACK;
use super::paths::{normalize_rel, rel_path};
use super::section::{SectionMatch, SectionProbe};
use super::snapshot::Snapshot;
use super::store::{Record, Row, RowKind};
use anyhow::{Context as _, Result};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

/// How far before the wall clock a record's `written_at` is put when the
/// filesystem's own clock cannot be read for it (the file sits on another
/// device than the `<home>/codegen/.clock` probe): far enough back that the
/// record stays racy, so it is always checked by hash.
///
/// A record's racy rule is git's, on the filesystem's clock: `written_at` is
/// the mtime of the probe settle writes after the bytes land, and a record
/// whose mtime is not strictly older than it (`mtime_ns >= written_at_ns`)
/// is checked by hash, never by stat. A file written in the same mtime tick
/// as the probe therefore stays hashed; one written in an earlier tick is
/// trusted by stat.
pub const RECORD_RACY_SLACK_NS: i64 = SECTION_RACY_SLACK.as_nanos() as i64;

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// Excluded from sources: heph wrote it, is writing it, or a line says so.
    Generated,
    /// Recorded, verified, no longer under any line: read as source.
    Released,
    /// Recorded, bytes differ, under a line: excluded, with a warning.
    Modified,
    /// Read as source.
    Source,
}

impl StatusKind {
    /// Whether readers leave the path out of their sources.
    pub fn is_excluded(self) -> bool {
        matches!(self, Self::Generated | Self::Modified)
    }
}

/// Why a path is what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusReason {
    /// A verified row.
    Recorded,
    /// A row with a write-back in flight, or one that crashed.
    Pending,
    /// A line matches and no row records it.
    Unrecorded,
    /// The row's bytes differ from what is on disk.
    Edited,
}

/// A lookup's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status<'s> {
    pub kind: StatusKind,
    /// `None` for `Source`.
    pub reason: Option<StatusReason>,
    /// The row that decided, if any (after a fold hit, the other casing's).
    pub row: Option<&'s Row>,
    /// How the section matched: on the row's recorded path after a fold hit.
    pub matched: SectionMatch,
}

impl Status<'_> {
    /// The deciding section line (index into `Section::lines`). `None` when
    /// none matched, or a walk inherited territory from a directory: `why`
    /// asks with a point lookup.
    pub fn line(&self) -> Option<usize> {
        self.matched.line()
    }

    fn source(row: Option<&Row>, matched: SectionMatch) -> Status<'_> {
        Status {
            kind: StatusKind::Source,
            reason: None,
            row,
            matched,
        }
    }
}

/// What the caller already knows about the path it looks up.
#[derive(Debug, Clone, Copy)]
pub struct LookupMeta {
    pub is_dir: bool,
    /// How the section is tested: a walk passes its carried territory.
    pub probe: SectionProbe,
    /// The path's `lstat`, when the caller has one (spares the lookup's).
    pub stat: Option<FileStat>,
}

impl LookupMeta {
    /// A lone path: `file()`, `why`, the guard.
    pub fn point(is_dir: bool) -> Self {
        Self {
            is_dir,
            probe: SectionProbe::Point,
            stat: None,
        }
    }

    /// An entry of a walk whose parent is (or is not) territory.
    pub fn walk(is_dir: bool, parent_territory: bool) -> Self {
        Self {
            is_dir,
            probe: SectionProbe::Walk { parent_territory },
            stat: None,
        }
    }

    pub fn with_stat(mut self, stat: FileStat) -> Self {
        self.stat = Some(stat);
        self
    }
}

impl Snapshot {
    /// What the root-relative, normalised path `rel` is (see the module docs).
    pub fn status(&self, rel: &[u8], meta: &LookupMeta) -> Result<Status<'_>> {
        let env = self.env();
        let abs = env.root.join(rel_path(rel));
        let mut walked = meta.stat;
        let (row, matched) = match self.row(rel) {
            Some(row) => (
                Some(row),
                self.section().probe(rel, meta.is_dir, meta.probe),
            ),
            None => match self.fold_hit(rel, &abs, &mut walked)? {
                // The section is tested on the recorded path, as a point
                // lookup: the walk's carried territory is the walked casing's.
                Some(row) => (Some(row), self.section().match_point(&row.path, false)),
                None => (None, self.section().probe(rel, meta.is_dir, meta.probe)),
            },
        };
        let territory = matched.is_territory();
        let Some(row) = row else {
            return Ok(if territory {
                Status {
                    kind: StatusKind::Generated,
                    reason: Some(StatusReason::Unrecorded),
                    row: None,
                    matched,
                }
            } else {
                Status::source(None, matched)
            });
        };
        if row.pending_hash.is_some() {
            return Ok(Status {
                kind: StatusKind::Generated,
                reason: Some(StatusReason::Pending),
                row: Some(row),
                matched,
            });
        }
        let stat = match walked {
            Some(s) => Some(s),
            None => lstat_opt(env.fs.as_ref(), &abs)?,
        };
        let status = |kind, reason| Status {
            kind,
            reason: Some(reason),
            row: Some(row),
            matched,
        };
        Ok(if verify(env.fs.as_ref(), row, stat.as_ref(), &abs)? {
            if !territory && self.section().is_present() && row.seen_in_section {
                status(StatusKind::Released, StatusReason::Recorded)
            } else {
                status(StatusKind::Generated, StatusReason::Recorded)
            }
        } else if territory {
            status(StatusKind::Modified, StatusReason::Edited)
        } else {
            Status::source(Some(row), matched)
        })
    }

    /// [`Self::status`] of any path, absolute or root-relative, normalised
    /// first; a path outside the root is `Source` without a matcher call.
    pub fn status_path(&self, path: &Path, is_dir: bool) -> Result<Status<'_>> {
        let Some(rel) = normalize_rel(&self.env().root, path) else {
            return Ok(Status::source(None, SectionMatch::None));
        };
        self.status(&rel, &LookupMeta::point(is_dir))
    }

    /// Lookup step 0's fold probe: the row recording another casing or
    /// normalisation of `rel` that is the same file, at both names.
    fn fold_hit(
        &self,
        rel: &[u8],
        abs: &Path,
        walked: &mut Option<FileStat>,
    ) -> Result<Option<&Row>> {
        if !self.has_fold_entries() {
            return Ok(None);
        }
        let candidates = self.fold_candidates(fold_hash(rel));
        if candidates.iter().all(|r| r.path == rel) {
            return Ok(None);
        }
        let fs = self.env().fs.as_ref();
        let st = match *walked {
            Some(s) => s,
            None => match lstat_opt(fs, abs)? {
                Some(s) => *walked.insert(s),
                None => return Ok(None),
            },
        };
        for row in candidates {
            // The walked file against the record's `(dev, ino)` first, so a
            // fold-key collision costs no second lstat. A new path's first
            // row has no record: its recorded path is the only witness.
            let maybe = row.path != rel
                && row
                    .record
                    .as_ref()
                    .is_none_or(|r| r.dev == st.dev && r.ino == st.ino);
            // Then the recorded name must still be that file: Linux
            // `mv gen/a.go Gen/a.go` keeps the inode but leaves no `gen/a.go`.
            if maybe && same_at_recorded(fs, &self.env().root, row, &st)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

/// Whether `row`'s recorded path lstats to the walked file's `(dev, ino)`.
fn same_at_recorded(fs: &dyn IndexFs, root: &Path, row: &Row, walked: &FileStat) -> Result<bool> {
    Ok(lstat_opt(fs, &root.join(rel_path(&row.path)))?.is_some_and(|s| s.same_inode(walked)))
}

/// Whether the stat alone proves `rec`: every compared field equal, and the
/// record outside its racy window.
pub(crate) fn stat_trusted(rec: &Record, st: &FileStat) -> bool {
    let racy = rec.mtime_ns >= rec.written_at_ns;
    !racy
        && st.size == rec.size
        && st.mtime_ns == rec.mtime_ns
        && st.ino == rec.ino
        && st.dev == rec.dev
        && st.mode == rec.mode
}

/// Whether what is at `abs` (with lstat `st`, `None` when absent) is what
/// `row` recorded. A file by stat, else by the cached content hash; a symlink
/// by its kind and `readlink`. A kind mismatch differs.
pub(crate) fn verify(
    fs: &dyn IndexFs,
    row: &Row,
    st: Option<&FileStat>,
    abs: &Path,
) -> Result<bool> {
    let Some(st) = st else {
        return Ok(false);
    };
    match row.kind {
        RowKind::Symlink => {
            if st.kind != StatKind::Symlink {
                return Ok(false);
            }
            let Some(want) = row.link_target.as_deref() else {
                return Ok(false);
            };
            Ok(read_link_opt(fs, abs)?.is_some_and(|t| t == want))
        }
        RowKind::File => {
            if st.kind != StatKind::File {
                return Ok(false);
            }
            let Some(rec) = row.record.as_ref() else {
                return Ok(false);
            };
            if stat_trusted(rec, st) {
                return Ok(true);
            }
            Ok(fs.content_hash(abs)? == rec.hash)
        }
    }
}

/// `readlink` as bytes, `None` when nothing is there.
pub(crate) fn read_link_opt(fs: &dyn IndexFs, abs: &Path) -> Result<Option<Vec<u8>>> {
    match fs.read_link(abs) {
        Ok(t) => Ok(Some(t.as_os_str().as_bytes().to_vec())),
        Err(e) if is_absent(&e) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("readlink {}", abs.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen_index::fold::fold_hash;
    use crate::codegen_index::fs::lstat;
    use crate::codegen_index::store::RowState;
    use crate::codegen_index::testfs::{Fake, TestFs, Ws, file_stat, hash, record, ws};
    use std::os::unix::fs::PermissionsExt as _;

    const ADDR: &str = "//p:gen";

    fn status(w: &Ws, rel: &str) -> (StatusKind, Option<StatusReason>) {
        let s = w.snap();
        let st = s.status(rel.as_bytes(), &LookupMeta::point(false)).unwrap();
        (st.kind, st.reason)
    }

    fn kind(w: &Ws, rel: &str) -> StatusKind {
        status(w, rel).0
    }

    /// A row recording `rel` with an injected identity, verified by stat.
    fn faked_row(w: &Ws, rel: &[u8], ino: u64) -> Row {
        let st = file_stat(3, 1_000_000_000, ino, 1);
        let mut r = Row::new(rel.to_vec(), ADDR, RowKind::File);
        r.record = Some(record(hash(b"abc"), &st, 100_000_000_000));
        w.fs.fake(&w.root.join(rel_path(rel)), Fake::Stat(st));
        r
    }

    fn fake(w: &Ws, rel: &[u8], ino: u64) {
        w.fs.fake(
            &w.root.join(rel_path(rel)),
            Fake::Stat(file_stat(3, 1_000_000_000, ino, 1)),
        );
    }

    #[test]
    fn stat_trusted_outside_racy_window() {
        let w = ws();
        w.write("gen/a.go", b"old");
        let mut row = w.recorded("gen/a.go", ADDR);
        // Same size, mtime and inode; different bytes: the stat is trusted (E1).
        if let Some(r) = row.record.as_mut() {
            r.hash = hash(b"new");
        }
        w.put(&[row]);
        w.section(&["/gen"]);
        assert_eq!(kind(&w, "gen/a.go"), StatusKind::Generated);
        assert_eq!(TestFs::count(&w.fs.content_hashes), 0);
    }

    #[test]
    fn racy_record_is_hashed() {
        let w = ws();
        w.write("gen/a.go", b"old");
        let mut row = w.recorded("gen/a.go", ADDR);
        if let Some(r) = row.record.as_mut() {
            r.hash = hash(b"new");
            // Recorded in the same tick as the mtime.
            r.written_at_ns = r.mtime_ns;
        }
        w.put(&[row]);
        w.section(&["/gen"]);
        assert_eq!(
            status(&w, "gen/a.go"),
            (StatusKind::Modified, Some(StatusReason::Edited))
        );
        assert_eq!(TestFs::count(&w.fs.content_hashes), 1);
    }

    #[test]
    fn same_bytes_new_inode_is_generated_after_one_hash() {
        let w = ws();
        w.write("gen/a.go", b"same");
        let mut row = w.recorded("gen/a.go", ADDR);
        if let Some(r) = row.record.as_mut() {
            r.ino += 1;
            r.mtime_ns -= 100_000_000_000;
        }
        w.put(&[row.clone()]);
        let g = w.generation();
        assert_eq!(
            status(&w, "gen/a.go"),
            (StatusKind::Generated, Some(StatusReason::Recorded))
        );
        assert_eq!(TestFs::count(&w.fs.content_hashes), 1);
        // Lookups never write: the row is not rewritten.
        assert_eq!(w.generation(), g);
        assert_eq!(w.db_row("gen/a.go").and_then(|r| r.record), row.record);
    }

    #[test]
    fn other_casing_same_inode_is_generated() {
        let w = ws();
        let row = faked_row(&w, b"gen/a.go", 42);
        w.put(&[row]);
        fake(&w, b"Gen/a.go", 42);
        let s = w.snap();
        let st = s.status(b"Gen/a.go", &LookupMeta::point(false)).unwrap();
        assert_eq!(st.kind, StatusKind::Generated);
        assert_eq!(st.row.map(|r| r.path.as_slice()), Some(&b"gen/a.go"[..]));
    }

    #[test]
    fn other_casing_other_inode_is_source() {
        let w = ws();
        w.section(&["/gen"]);
        let row = faked_row(&w, b"gen/a.go", 42);
        w.put(&[row]);
        fake(&w, b"Gen/a.go", 43);
        assert_eq!(status(&w, "Gen/a.go"), (StatusKind::Source, None));
    }

    #[test]
    fn linux_casing_rename_same_inode_is_a_miss() {
        let w = ws();
        w.section(&["/gen"]);
        let row = faked_row(&w, b"gen/a.go", 42);
        w.put(&[row]);
        // `mv gen/a.go Gen/a.go`: the inode moved, the recorded path is gone.
        fake(&w, b"Gen/a.go", 42);
        w.fs.fake(&w.abs("gen/a.go"), Fake::Absent);
        assert_eq!(status(&w, "Gen/a.go"), (StatusKind::Source, None));
    }

    #[test]
    fn fold_hit_matches_section_on_recorded_path() {
        let w = ws();
        w.section(&["/gen"]);
        let row = faked_row(&w, b"gen/a.go", 42);
        w.put(&[row]);
        fake(&w, b"Gen/a.go", 42);
        let s = w.snap();
        for meta in [LookupMeta::point(false), LookupMeta::walk(false, false)] {
            let st = s.status(b"Gen/a.go", &meta).unwrap();
            assert_eq!(st.kind, StatusKind::Generated, "{meta:?}");
            assert_eq!(st.line(), Some(0), "the section tested on gen/a.go");
        }
    }

    #[test]
    fn both_casings_recorded_each_found_and_walked_compared_with_both() {
        let w = ws();
        let upper = faked_row(&w, b"A.go", 1);
        let lower = faked_row(&w, b"a.go", 2);
        w.put(&[upper, lower]);
        let s = w.snap();
        let at = |p: &[u8]| {
            s.status(p, &LookupMeta::point(false))
                .unwrap()
                .row
                .map(|r| r.path.clone())
        };
        assert_eq!(at(b"A.go"), Some(b"A.go".to_vec()));
        assert_eq!(at(b"a.go"), Some(b"a.go".to_vec()));
        fake(&w, b"a.GO", 2);
        assert_eq!(at(b"a.GO"), Some(b"a.go".to_vec()));
    }

    #[test]
    fn fold_key_collision_costs_one_lstat() {
        let w = ws();
        let mut row = faked_row(&w, b"y.go", 7);
        // Another path under x.go's fold key: a collision.
        row.fold = fold_hash(b"x.go");
        w.index.publish(w.generation() + 1, &[row], &[]);
        fake(&w, b"x.go", 8);
        let before = TestFs::count(&w.fs.lstats);
        assert_eq!(status(&w, "x.go"), (StatusKind::Source, None));
        assert_eq!(TestFs::count(&w.fs.lstats) - before, 1);
    }

    #[test]
    fn reused_inode_at_other_casing_is_never_a_wrong_generated() {
        let w = ws();
        w.section(&["/gen"]);
        w.write("Gen/a.go", b"different");
        let row = faked_row(&w, b"gen/a.go", 42);
        w.put(&[row]);
        // The walked file has the row's inode, but not its size: hashed.
        let st = FileStat {
            size: 9,
            ..file_stat(9, 2_000_000_000, 42, 1)
        };
        w.fs.fake(&w.abs("Gen/a.go"), Fake::Stat(st));
        assert_eq!(kind(&w, "Gen/a.go"), StatusKind::Modified);
        // And with the recorded path gone, a miss.
        w.fs.fake(&w.abs("gen/a.go"), Fake::Absent);
        assert_eq!(kind(&w, "Gen/a.go"), StatusKind::Source);
    }

    #[test]
    fn nfd_walked_name_finds_nfc_row() {
        let w = ws();
        let nfc = "\u{e9}.go".as_bytes();
        let nfd = "e\u{301}.go".as_bytes();
        let row = faked_row(&w, nfc, 5);
        w.put(&[row]);
        fake(&w, nfd, 5);
        let s = w.snap();
        let st = s.status(nfd, &LookupMeta::point(false)).unwrap();
        assert_eq!(st.kind, StatusKind::Generated);
        assert_eq!(st.row.map(|r| r.path.as_slice()), Some(nfc));
    }

    #[test]
    fn simple_case_folding_without_locale_is_a_miss() {
        let w = ws();
        for (recorded, walked) in [("straße.go", "STRASSE.go"), ("İ.go", "i.go")] {
            assert_ne!(fold_hash(recorded.as_bytes()), fold_hash(walked.as_bytes()));
            let row = faked_row(&w, recorded.as_bytes(), 9);
            w.put(&[row]);
            fake(&w, walked.as_bytes(), 9);
            let before = TestFs::count(&w.fs.lstats);
            assert_eq!(kind(&w, walked), StatusKind::Source, "{walked}");
            assert_eq!(
                TestFs::count(&w.fs.lstats),
                before,
                "no candidate, no lstat"
            );
        }
    }

    #[test]
    fn empty_index_costs_no_syscall() {
        let w = ws();
        assert_eq!(status(&w, "a.go"), (StatusKind::Source, None));
        assert_eq!(TestFs::count(&w.fs.lstats), 0);
    }

    #[test]
    fn non_normalised_paths_are_normalised_before_the_probe() {
        let w = ws();
        w.write("a/b", b"x");
        w.write("a1", b"y");
        let rows = [w.recorded("a/b", ADDR), w.recorded("a1", ADDR)];
        w.put(&rows);
        let s = w.snap();
        for (p, want) in [
            ("./a1", "a1"),
            ("a//b", "a/b"),
            ("x/../a1", "a1"),
            ("a/./b", "a/b"),
        ] {
            let st = s.status_path(Path::new(p), false).unwrap();
            assert_eq!(st.kind, StatusKind::Generated, "{p}");
            assert_eq!(st.row.map(|r| r.path.as_slice()), Some(want.as_bytes()));
        }
        let abs = s.status_path(&w.abs("a1"), false).unwrap();
        assert_eq!(abs.kind, StatusKind::Generated);
        let outside = s.status_path(Path::new("/elsewhere/a1"), false).unwrap();
        assert_eq!((outside.kind, outside.row), (StatusKind::Source, None));
    }

    #[test]
    fn non_utf8_name_compared_as_bytes_folded_by_ascii() {
        let w = ws();
        let rel = b"gen/\xff.go";
        assert_eq!(fold_hash(rel), fold_hash(b"GEN/\xff.GO"));
        let row = faked_row(&w, rel, 11);
        w.put(&[row]);
        assert_eq!(
            w.snap()
                .status(rel, &LookupMeta::point(false))
                .unwrap()
                .kind,
            StatusKind::Generated
        );
        fake(&w, b"GEN/\xff.GO", 11);
        let s = w.snap();
        let st = s.status(b"GEN/\xff.GO", &LookupMeta::point(false)).unwrap();
        assert_eq!(st.row.map(|r| r.path.as_slice()), Some(&rel[..]));
    }

    #[test]
    fn symlink_where_a_file_was_recorded_differs_by_kind() {
        let w = ws();
        w.write("gen/a.go", b"x");
        let row = w.recorded("gen/a.go", ADDR);
        w.put(&[row]);
        std::fs::remove_file(w.abs("gen/a.go")).unwrap();
        std::os::unix::fs::symlink("elsewhere", w.abs("gen/a.go")).unwrap();
        assert_eq!(kind(&w, "gen/a.go"), StatusKind::Source);
        w.section(&["/gen"]);
        assert_eq!(kind(&w, "gen/a.go"), StatusKind::Modified);
    }

    #[test]
    fn symlink_row_verified_by_lstat_and_readlink() {
        let w = ws();
        w.section(&["/gen"]);
        std::fs::create_dir_all(w.abs("gen")).unwrap();
        std::os::unix::fs::symlink("target-a", w.abs("gen/l")).unwrap();
        let st = lstat(&w.abs("gen/l")).unwrap();
        let mut row = Row::new(b"gen/l".to_vec(), ADDR, RowKind::Symlink);
        row.link_target = Some(b"target-a".to_vec());
        row.record = Some(record(hash(b"target-a"), &st, st.mtime_ns));
        w.put(&[row]);
        assert_eq!(kind(&w, "gen/l"), StatusKind::Generated);
        assert_eq!(TestFs::count(&w.fs.content_hashes), 0, "never hashed");
        std::fs::remove_file(w.abs("gen/l")).unwrap();
        std::os::unix::fs::symlink("target-b", w.abs("gen/l")).unwrap();
        assert_eq!(kind(&w, "gen/l"), StatusKind::Modified);
        // A regular file where the symlink was recorded differs by kind.
        std::fs::remove_file(w.abs("gen/l")).unwrap();
        w.write("gen/l", b"target-a");
        assert_eq!(kind(&w, "gen/l"), StatusKind::Modified);
    }

    #[test]
    fn exec_bit_flipped_is_modified() {
        let w = ws();
        w.section(&["/gen"]);
        w.write("gen/a.sh", b"echo");
        let row = w.recorded("gen/a.sh", ADDR);
        w.put(&[row]);
        assert_eq!(kind(&w, "gen/a.sh"), StatusKind::Generated);
        std::fs::set_permissions(w.abs("gen/a.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(kind(&w, "gen/a.sh"), StatusKind::Modified);
    }

    #[test]
    fn zero_byte_file_verified_like_any_other() {
        let w = ws();
        w.write("gen/empty", b"");
        let row = w.recorded("gen/empty", ADDR);
        w.put(&[row]);
        assert_eq!(kind(&w, "gen/empty"), StatusKind::Generated);
        w.write("gen/empty", b"now not");
        assert_eq!(kind(&w, "gen/empty"), StatusKind::Source);
    }

    #[test]
    fn mtime_before_the_epoch_compared_as_recorded() {
        let w = ws();
        w.write("gen/old", b"abc");
        let st = file_stat(3, -5_000_000_000, 77, 1);
        w.fs.fake(&w.abs("gen/old"), Fake::Stat(st));
        let mut row = Row::new(b"gen/old".to_vec(), ADDR, RowKind::File);
        row.record = Some(record(hash(b"zzz"), &st, 1_000_000_000_000));
        w.put(&[row.clone()]);
        // Outside the racy window: the stat is trusted.
        assert_eq!(kind(&w, "gen/old"), StatusKind::Generated);
        // Recorded in its own tick: hashed, and the bytes differ.
        row.record = Some(record(hash(b"zzz"), &st, -5_000_000_000));
        w.put(&[row]);
        assert_eq!(kind(&w, "gen/old"), StatusKind::Source);
        assert_eq!(TestFs::count(&w.fs.content_hashes), 1);
    }

    #[test]
    fn pending_row_is_generated_whatever_the_bytes() {
        let w = ws();
        w.write("gen/p", b"partial");
        let mut row = w.recorded("gen/p", ADDR);
        row.pending_hash = Some(hash(b"full"));
        let mut absent = Row::new(b"gen/q".to_vec(), ADDR, RowKind::File);
        absent.pending_hash = Some(hash(b"q"));
        w.put(&[row, absent]);
        assert_eq!(
            status(&w, "gen/p"),
            (StatusKind::Generated, Some(StatusReason::Pending))
        );
        // No file: nothing to classify, the row stays pending.
        assert_eq!(
            status(&w, "gen/q"),
            (StatusKind::Generated, Some(StatusReason::Pending))
        );
        assert_eq!(TestFs::count(&w.fs.lstats), 0);
        assert!(w.db_row("gen/q").is_some_and(|r| r.pending_hash.is_some()));
    }

    #[test]
    fn row_whose_path_is_now_a_directory_is_source() {
        let w = ws();
        w.write("gen/d", b"x");
        let row = w.recorded("gen/d", ADDR);
        w.put(&[row]);
        std::fs::remove_file(w.abs("gen/d")).unwrap();
        std::fs::create_dir(w.abs("gen/d")).unwrap();
        let s = w.snap();
        let st = s.status(b"gen/d", &LookupMeta::point(true)).unwrap();
        assert_eq!(st.kind, StatusKind::Source);
        assert!(w.db_row("gen/d").is_some(), "the row stays");
    }

    #[test]
    fn imported_row_with_equal_bytes_is_generated() {
        let w = ws();
        w.write("gen/i", b"x");
        let mut row = w.recorded("gen/i", ADDR);
        row.state = RowState::Imported;
        row.output = None;
        w.put(&[row]);
        assert_eq!(
            status(&w, "gen/i"),
            (StatusKind::Generated, Some(StatusReason::Recorded))
        );
    }

    #[test]
    fn empty_index_no_section_is_source_and_unrecorded_under_a_line() {
        let w = ws();
        w.write("gen/a.go", b"x");
        assert_eq!(status(&w, "gen/a.go"), (StatusKind::Source, None));
        w.section(&["/gen"]);
        assert_eq!(
            status(&w, "gen/a.go"),
            (StatusKind::Generated, Some(StatusReason::Unrecorded))
        );
    }

    #[test]
    fn released_only_with_a_section_and_seen() {
        let w = ws();
        w.write("gen/a.go", b"x");
        let mut row = w.recorded("gen/a.go", ADDR);
        w.put(&[row.clone()]);
        // No section at all: still generated.
        assert_eq!(kind(&w, "gen/a.go"), StatusKind::Generated);
        w.section(&["/other"]);
        assert_eq!(kind(&w, "gen/a.go"), StatusKind::Generated, "never seen");
        row.seen_in_section = true;
        w.put(&[row]);
        assert_eq!(
            status(&w, "gen/a.go"),
            (StatusKind::Released, Some(StatusReason::Recorded))
        );
        // A negating line: released, and the line is named.
        w.section(&["/gen/*", "!/gen/a.go"]);
        let s = w.snap();
        let st = s.status(b"gen/a.go", &LookupMeta::point(false)).unwrap();
        assert_eq!((st.kind, st.line()), (StatusKind::Released, Some(1)));
    }

    #[test]
    fn stale_row_after_foreign_rewrite_is_modified_or_source() {
        let w = ws();
        w.write("gen/a.go", b"v1 written by N");
        let row = w.recorded("gen/a.go", ADDR);
        w.put(&[row]);
        // N-1 rewrites the file without touching the index.
        std::fs::remove_file(w.abs("gen/a.go")).unwrap();
        w.write("gen/a.go", b"v2 written by N-1");
        assert_eq!(kind(&w, "gen/a.go"), StatusKind::Source);
        w.section(&["/gen"]);
        assert_eq!(
            status(&w, "gen/a.go"),
            (StatusKind::Modified, Some(StatusReason::Edited))
        );
    }
}
