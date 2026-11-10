//! A test [`IndexFs`]: the real filesystem, with injectable `lstat` answers
//! (case-folded names, shared or reused inodes cannot be staged on every
//! host) and counters for the calls a lookup must not make.

use super::fs::{FileStat, IndexFs, StatKind, lstat};
use super::snapshot::Snapshot;
use super::store::{Record, Row, RowKind};
use super::{CodegenIndex, ImportLockGuard, TargetLockGuard};
use crate::gitignore_section::{BEGIN_MARKER, END_MARKER};
use anyhow::Result;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// An injected `lstat` answer.
#[derive(Debug, Clone, Copy)]
pub enum Fake {
    /// `lstat` returns this; hashing still reads the real file (if any).
    Stat(FileStat),
    /// `lstat` reports nothing there.
    Absent,
}

#[derive(Debug, Default)]
pub struct TestFs {
    fakes: Mutex<FxHashMap<PathBuf, Fake>>,
    pub lstats: AtomicU64,
    pub content_hashes: AtomicU64,
    pub fresh_hashes: AtomicU64,
}

impl TestFs {
    pub fn fake(&self, path: &Path, fake: Fake) {
        self.fakes.lock().insert(path.to_path_buf(), fake);
    }

    pub fn count(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// A regular file's stat with the given identity.
pub fn file_stat(size: u64, mtime_ns: i64, ino: u64, dev: u64) -> FileStat {
    FileStat {
        kind: StatKind::File,
        size,
        mtime_ns,
        ino,
        dev,
        mode: 0o100_644,
    }
}

/// A temp workspace with an index (and its db) over a [`TestFs`].
pub struct Ws {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub fs: Arc<TestFs>,
    pub index: Arc<CodegenIndex>,
}

pub fn ws() -> Ws {
    let w = ws_no_db();
    w.index
        .create_db(&ImportLockGuard::new(Box::new(())))
        .unwrap();
    w
}

pub fn ws_no_db() -> Ws {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let fs = Arc::new(TestFs::default());
    let index = CodegenIndex::open(&root, &root.join(".heph"), fs.clone()).unwrap();
    Ws {
        _dir: dir,
        root,
        fs,
        index: Arc::new(index),
    }
}

pub fn hash(bytes: &[u8]) -> String {
    crate::bytes_hashout(bytes, false)
}

pub fn guard(addr: &str) -> TargetLockGuard {
    TargetLockGuard::new(addr, Box::new(()))
}

impl Ws {
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.abs(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(p, bytes).unwrap();
    }

    pub fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.abs(rel)).ok()
    }

    /// A `.gitignore` whose managed section holds `lines`.
    pub fn section(&self, lines: &[&str]) {
        let mut text = format!("{BEGIN_MARKER}\n");
        for l in lines {
            text.push_str(l);
            text.push('\n');
        }
        text.push_str(END_MARKER);
        text.push('\n');
        std::fs::write(self.root.join(".gitignore"), text).unwrap();
    }

    pub fn snap(&self) -> Snapshot {
        self.index.snapshot().unwrap()
    }

    pub fn generation(&self) -> u64 {
        self.index.store().unwrap().generation().unwrap()
    }

    /// Commit `rows` and publish them.
    pub fn put(&self, rows: &[Row]) {
        let c = self
            .index
            .store()
            .unwrap()
            .write(|t| {
                for r in rows {
                    t.put(r)?;
                }
                Ok(())
            })
            .unwrap();
        self.index.publish_committed(&c);
    }

    #[expect(
        clippy::unwrap_in_result,
        reason = "a test helper: a db error fails the test"
    )]
    pub fn db_row(&self, rel: &str) -> Option<Row> {
        self.index.store().unwrap().row(rel.as_bytes()).unwrap()
    }

    /// A `written` row of `addr` recording the file now at `rel`: its real
    /// stat and hash, taken well after its mtime (outside the racy window).
    pub fn recorded(&self, rel: &str, addr: &str) -> Row {
        let st = lstat(&self.abs(rel)).unwrap();
        let h = crate::file_hashout(&self.abs(rel), st.exec()).unwrap();
        let mut r = Row::new(rel.as_bytes().to_vec(), addr, RowKind::File);
        r.output = Some("out".into());
        r.record = Some(record(h, &st, st.mtime_ns + 10_000_000_000));
        r
    }
}

pub fn record(hash: String, st: &FileStat, written_at_ns: i64) -> Record {
    Record {
        hash,
        size: st.size,
        mtime_ns: st.mtime_ns,
        ino: st.ino,
        dev: st.dev,
        mode: st.mode,
        written_at_ns,
    }
}

impl IndexFs for TestFs {
    fn lstat(&self, path: &Path) -> io::Result<FileStat> {
        self.lstats.fetch_add(1, Ordering::Relaxed);
        match self.fakes.lock().get(path) {
            Some(Fake::Stat(s)) => Ok(*s),
            Some(Fake::Absent) => Err(io::Error::from(io::ErrorKind::NotFound)),
            None => lstat(path),
        }
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(path)
    }

    fn content_hash(&self, path: &Path) -> Result<String> {
        self.content_hashes.fetch_add(1, Ordering::Relaxed);
        let x = lstat(path)?.exec();
        crate::file_hashout(path, x)
    }

    fn fresh_hash(&self, path: &Path, x: bool) -> Result<String> {
        self.fresh_hashes.fetch_add(1, Ordering::Relaxed);
        crate::file_hashout(path, x)
    }

    fn unlink(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}
