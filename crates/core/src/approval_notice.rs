//! Approval notices, read from the artifact store.
//!
//! No notice is ever written to disk, and none is held whole in memory. At
//! gate time the engine records, per notice, where its text is kept — a
//! [`NoticeSource`] per artifact of the input groups it names — after moving
//! any of them that lived only in the run's memory (an uncached input) into
//! heph's local cache. Every reader, the live approval view included, renders
//! the text from those sources, read-only, through a [`ReadNotice`] the host
//! supplies (the engine's, which this crate cannot reach), and stops at
//! [`NOTICE_READ_CAP`] bytes. A source it cannot read reads as
//! [`NOTICE_UNAVAILABLE`].
//!
//! **Diagnostics only.** A notice is shown to a person deciding an approval;
//! nothing here reaches a cache key, a def hash, a sandbox, or a target's env,
//! argv or files.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a reader shows in place of a notice it cannot render: an artifact is
/// not in the local store (a gc'd or a remote-only revision), a source file
/// changed or went away, or the record names no source at all.
pub const NOTICE_UNAVAILABLE: &str = "notice unavailable";

/// The most of a notice's text a reader renders, in bytes. Past it the text
/// ends with a line saying how much there was and where to read all of it, so
/// a notice whose input is a large artifact is never loaded whole.
pub const NOTICE_READ_CAP: usize = 4 << 20;

/// One artifact a notice is rendered from: the input target it was resolved
/// for, as the gated target declared it, and where its bytes are kept. A
/// notice's text is its sources' files, concatenated in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeSource {
    /// The input target. Named in the truncation line, as what to inspect
    /// for the full text.
    pub addr: String,
    pub origin: NoticeOrigin,
}

/// Where a [`NoticeSource`]'s bytes are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoticeOrigin {
    /// The entry whose hashout is `hashout` in revision `revision` of target
    /// `addr` in the local cache. `revision` is the hashin, or for an uncached
    /// input its `{hashin}_{nanos}` key — which no later run looks up, so it
    /// is never a cache hit.
    Cache {
        addr: String,
        revision: String,
        hashout: String,
    },
    /// A source file read in place — the user's file, never copied into a
    /// cache. `path` is relative to the workspace root; the file reads as the
    /// single entry `out_path`, and only while it still hashes to `hashout`.
    File {
        path: String,
        out_path: String,
        x: bool,
        hashout: String,
    },
    /// A path-rewriting view of `source` (a `group` target's relocation):
    /// the `(source path, path)` pairs it keeps, in source order.
    View {
        source: Box<NoticeOrigin>,
        paths: Vec<(String, String)>,
    },
}

/// Where a reader finds a notice's sources: heph's `home` (its local cache)
/// and the workspace `root` (source files read in place).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeRoots {
    pub home: PathBuf,
    pub root: PathBuf,
}

/// Render a notice from its sources, read-only — no build, no fetch, no
/// write — capped at [`NOTICE_READ_CAP`] bytes. `None` — show
/// [`NOTICE_UNAVAILABLE`] — when `sources` is empty or any of them cannot be
/// read. Blocking: call it off the render task.
pub type ReadNotice = fn(roots: &NoticeRoots, sources: &[NoticeSource]) -> Option<String>;
