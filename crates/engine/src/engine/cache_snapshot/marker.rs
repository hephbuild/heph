//! The snapshot's `HEPH_SNAPSHOT` marker: which generation under `<dir>` is
//! current.
//!
//! ```text
//! <dir>/HEPH_SNAPSHOT       {"format": 1, "heph": "<version>", "current": "gen-<nonce>"}
//! <dir>/gen-<nonce>/...     the remote wire format
//! ```
//!
//! - `current` must be one path segment, `gen-` then `[A-Za-z0-9_-]+`, naming
//!   a directory under `<dir>`, or the marker is unreadable.
//! - A reader ignores fields it does not know, so adding one needs no format
//!   bump, and serves every `format` up to [`SNAPSHOT_FORMAT`].
//! - A missing, unreadable or newer marker means no tier: the build runs
//!   cold, with one warning carrying the format seen ([`open_tier`]). It never
//!   fails a build (I3).
//! - The marker is written last, by rename ([`write_marker`]), so a crash
//!   leaves the previous generation current.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The marker's file name under the snapshot dir.
pub const MARKER_FILE: &str = "HEPH_SNAPSHOT";

/// The prefix of the marker's temp files (`.HEPH_SNAPSHOT.<nonce>.tmp`),
/// which the snapshot command's sweep removes.
pub const MARKER_TMP_PREFIX: &str = ".HEPH_SNAPSHOT.";

/// The prefix of a generation directory's name (`gen-<nonce>`).
pub const GENERATION_PREFIX: &str = "gen-";

/// The marker format this heph writes, and the newest it reads.
///
/// **A bump is release-gated**, like `COREUTILS_VERSION`: every snapshot an
/// older heph wrote stays readable, but one written by the bumped heph is a
/// newer format to every older one, and a CI that runs both turns cold for
/// one job — as a miss, never a misread. A new field does not need a bump:
/// readers ignore fields they do not know.
pub const SNAPSHOT_FORMAT: u32 = 1;

/// The marker's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// The layout version; see [`SNAPSHOT_FORMAT`].
    pub format: u32,
    /// The heph that wrote the snapshot, for diagnosis only.
    #[serde(default)]
    pub heph: String,
    /// The current generation's directory name, `gen-<nonce>`.
    pub current: String,
    /// Whether the run that wrote it was incomplete, so it also carried over
    /// what it restored: a small snapshot then reads as "an earlier job
    /// failed", not "the restore missed". For diagnosis only.
    #[serde(default)]
    pub incomplete: bool,
}

/// The generation a readable marker names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    /// `<dir>/<current>`: the `file://` root of the snapshot tier.
    pub path: PathBuf,
    pub marker: Marker,
}

/// Why a snapshot dir gives no tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoTier {
    /// `<dir>` does not exist: the first job of a lineage.
    NoDir,
    /// `<dir>` exists without a marker.
    NoMarker,
    /// The marker could not be read, did not parse, or named no generation.
    /// `format` is what it said, when it said one.
    Unreadable { format: Option<u64>, reason: String },
    /// The marker is a newer format than this heph reads.
    Newer { format: u64 },
}

impl NoTier {
    /// The format the marker declared, when it was read far enough to say.
    pub fn format(&self) -> Option<u64> {
        match self {
            Self::NoDir | Self::NoMarker => None,
            Self::Unreadable { format, .. } => *format,
            Self::Newer { format } => Some(*format),
        }
    }
}

impl std::fmt::Display for NoTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDir => f.write_str("the snapshot directory does not exist"),
            Self::NoMarker => write!(f, "no {MARKER_FILE} marker"),
            Self::Unreadable {
                format: Some(n),
                reason,
            } => write!(f, "unreadable {MARKER_FILE} marker (format {n}): {reason}"),
            Self::Unreadable {
                format: None,
                reason,
            } => write!(f, "unreadable {MARKER_FILE} marker: {reason}"),
            Self::Newer { format } => write!(
                f,
                "{MARKER_FILE} marker format {format} is newer than this heph reads \
                 ({SNAPSHOT_FORMAT})"
            ),
        }
    }
}

/// Whether `name` is a generation directory's name: one path segment,
/// [`GENERATION_PREFIX`] then at least one of `[A-Za-z0-9_-]`.
pub fn is_generation_name(name: &str) -> bool {
    name.strip_prefix(GENERATION_PREFIX).is_some_and(|nonce| {
        !nonce.is_empty()
            && nonce
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// Read `dir`'s marker and the generation it names.
pub fn read_marker(dir: &Path) -> Result<Generation, NoTier> {
    let path = dir.join(MARKER_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(if dir.is_dir() {
                NoTier::NoMarker
            } else {
                NoTier::NoDir
            });
        }
        Err(e) => {
            return Err(NoTier::Unreadable {
                format: None,
                reason: e.to_string(),
            });
        }
    };
    // The format first, from the raw JSON, so a marker this heph cannot read
    // still reports the format it declared.
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| NoTier::Unreadable {
            format: None,
            reason: e.to_string(),
        })?;
    let format = raw.get("format").and_then(serde_json::Value::as_u64);
    let Some(format) = format else {
        return Err(NoTier::Unreadable {
            format: None,
            reason: "no numeric `format`".to_owned(),
        });
    };
    if format > u64::from(SNAPSHOT_FORMAT) {
        return Err(NoTier::Newer { format });
    }
    let unreadable = |reason: String| NoTier::Unreadable {
        format: Some(format),
        reason,
    };
    let marker: Marker = serde_json::from_value(raw).map_err(|e| unreadable(e.to_string()))?;
    if !is_generation_name(&marker.current) {
        return Err(unreadable(format!(
            "`current` {:?} is not a `{GENERATION_PREFIX}<nonce>` name",
            marker.current
        )));
    }
    let generation = dir.join(&marker.current);
    // `symlink_metadata`: a generation is a real directory under `<dir>`, never
    // a link out of it.
    match std::fs::symlink_metadata(&generation) {
        Ok(m) if m.is_dir() => Ok(Generation {
            path: generation,
            marker,
        }),
        Ok(_) => Err(unreadable(format!(
            "generation {} is not a directory",
            marker.current
        ))),
        Err(e) => Err(unreadable(format!("generation {}: {e}", marker.current))),
    }
}

/// The snapshot tier under `dir`, or `None` with the one diagnostic saying
/// why: a `warn!` carrying the format seen, except for a `dir` that does not
/// exist, which is a lineage's first job and only logged at `info`.
pub fn open_tier(dir: &Path) -> Option<Generation> {
    match read_marker(dir) {
        Ok(generation) => {
            tracing::debug!(
                dir = %dir.display(),
                generation = %generation.marker.current,
                format = generation.marker.format,
                heph = %generation.marker.heph,
                "cache snapshot: serving",
            );
            Some(generation)
        }
        Err(NoTier::NoDir) => {
            tracing::info!(dir = %dir.display(), "cache snapshot: no snapshot yet, building cold");
            None
        }
        Err(reason) => {
            tracing::warn!(
                dir = %dir.display(),
                format = ?reason.format(),
                reason = %reason,
                "cache snapshot: not used, building cold",
            );
            None
        }
    }
}

/// Point `dir`'s marker at the generation `current`, by writing a temp file
/// and renaming it over [`MARKER_FILE`]. The generation must already be
/// complete: this is the last write of a snapshot. `incomplete` records
/// whether the run that wrote it was ([`Marker::incomplete`]).
pub fn write_marker(dir: &Path, current: &str, incomplete: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        is_generation_name(current),
        "{current:?} is not a `{GENERATION_PREFIX}<nonce>` generation name"
    );
    let marker = Marker {
        format: SNAPSHOT_FORMAT,
        heph: hcore::version::current().to_owned(),
        current: current.to_owned(),
        incomplete,
    };
    let bytes = serde_json::to_vec(&marker).context("encoding the snapshot marker")?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let tmp = dir.join(format!("{MARKER_TMP_PREFIX}{nonce}.tmp"));
    let written = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        let path = dir.join(MARKER_FILE);
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
    })();
    if written.is_err() {
        // Best effort: the sweep removes a leftover temp file too.
        drop(std::fs::remove_file(&tmp));
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_raw(dir: &Path, body: &str) {
        std::fs::write(dir.join(MARKER_FILE), body).expect("write marker");
    }

    fn mkgen(dir: &Path, name: &str) {
        std::fs::create_dir(dir.join(name)).expect("create generation");
    }

    #[test]
    fn missing() {
        let tmp = snapshot_dir();
        let absent = tmp.path().join("nope");
        assert_eq!(read_marker(&absent), Err(NoTier::NoDir));
        assert_eq!(open_tier(&absent), None);
        assert_eq!(read_marker(tmp.path()), Err(NoTier::NoMarker));
        assert_eq!(open_tier(tmp.path()), None);
    }

    #[test]
    fn unparseable() {
        let tmp = snapshot_dir();
        for body in ["", "{", "not json", "[1]", "{\"current\":\"gen-a\"}"] {
            write_raw(tmp.path(), body);
            let err = read_marker(tmp.path()).expect_err(body);
            assert!(
                matches!(err, NoTier::Unreadable { format: None, .. }),
                "{body:?}: {err:?}"
            );
        }
        // The format parsed, the rest did not: the reason still carries it.
        write_raw(tmp.path(), "{\"format\":1,\"current\":7}");
        let err = read_marker(tmp.path()).expect_err("current is not a string");
        assert_eq!(err.format(), Some(1));
        assert!(err.to_string().contains("format 1"), "{err}");
    }

    #[test]
    fn newer_format_is_no_tier_with_the_format_in_the_reason() {
        let tmp = snapshot_dir();
        mkgen(tmp.path(), "gen-a");
        write_raw(
            tmp.path(),
            "{\"format\":2,\"current\":\"gen-a\",\"layout\":\"v9\"}",
        );
        let err = read_marker(tmp.path()).expect_err("newer");
        assert_eq!(err, NoTier::Newer { format: 2 });
        assert!(err.to_string().contains("format 2"), "{err}");
        assert_eq!(open_tier(tmp.path()), None);
    }

    #[test]
    fn older_or_equal_format_is_served() {
        let tmp = snapshot_dir();
        mkgen(tmp.path(), "gen-a");
        for format in 0..=SNAPSHOT_FORMAT {
            write_raw(
                tmp.path(),
                &format!("{{\"format\":{format},\"current\":\"gen-a\"}}"),
            );
            let generation = read_marker(tmp.path()).expect("served");
            assert_eq!(generation.path, tmp.path().join("gen-a"));
            assert_eq!(generation.marker.format, format);
            assert_eq!(generation.marker.heph, "", "heph defaults when absent");
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let tmp = snapshot_dir();
        mkgen(tmp.path(), "gen-a");
        write_raw(
            tmp.path(),
            "{\"format\":1,\"heph\":\"v1.2.3\",\"current\":\"gen-a\",\"later\":{\"x\":[1]}}",
        );
        let generation = read_marker(tmp.path()).expect("served");
        assert_eq!(generation.marker.heph, "v1.2.3");
        assert_eq!(generation.marker.current, "gen-a");
    }

    #[test]
    fn write_then_read_round_trips() {
        let tmp = snapshot_dir();
        mkgen(tmp.path(), "gen-0123abc_-Z");
        write_marker(tmp.path(), "gen-0123abc_-Z", false).expect("write");
        let generation = open_tier(tmp.path()).expect("served");
        assert_eq!(generation.path, tmp.path().join("gen-0123abc_-Z"));
        assert_eq!(
            generation.marker,
            Marker {
                format: SNAPSHOT_FORMAT,
                heph: hcore::version::current().to_owned(),
                current: "gen-0123abc_-Z".to_owned(),
                incomplete: false,
            }
        );
        // A second write replaces the first and leaves no temp file behind.
        mkgen(tmp.path(), "gen-b");
        write_marker(tmp.path(), "gen-b", true).expect("rewrite");
        let marker = read_marker(tmp.path()).expect("served").marker;
        assert_eq!(marker.current, "gen-b");
        assert!(marker.incomplete, "the incomplete flag round-trips");
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(MARKER_TMP_PREFIX))
            .collect();
        assert!(names.is_empty(), "{names:?}");
        // A name that is not a generation is refused before anything is written.
        write_marker(tmp.path(), "../x", false).expect_err("not a generation");
        assert_eq!(
            read_marker(tmp.path()).expect("served").marker.current,
            "gen-b"
        );
    }

    mod degrades {
        use super::*;

        /// `current` naming anything but one `gen-<nonce>` segment under the
        /// dir is an unreadable marker, never a tier rooted elsewhere.
        #[test]
        fn current_outside_dir() {
            let tmp = snapshot_dir();
            let dir = tmp.path().join("snap");
            std::fs::create_dir(&dir).expect("snap dir");
            // Every one of these exists, so only the name rule refuses them.
            std::fs::create_dir(tmp.path().join("x")).expect("x");
            std::fs::create_dir(tmp.path().join("gen-out")).expect("gen-out");
            mkgen(&dir, "gen-a");
            std::fs::create_dir(dir.join("gen-a").join("b")).expect("gen-a/b");
            for current in [
                "../x",
                "../gen-out",
                "gen-a/b",
                "gen-a/../gen-a",
                "gen-",
                "gen-a/",
                "/abs",
                "x",
                ".",
                "",
                "gen-\u{e9}",
            ] {
                let body = serde_json::json!({"format": 1, "current": current}).to_string();
                write_raw(&dir, &body);
                let err = read_marker(&dir).expect_err(current);
                assert!(
                    matches!(
                        err,
                        NoTier::Unreadable {
                            format: Some(1),
                            ..
                        }
                    ),
                    "{current:?}: {err:?}"
                );
            }
        }

        /// A well-formed name whose generation is gone (or is a file, or a
        /// link out of the dir) is unreadable too.
        #[test]
        fn current_names_no_generation_dir() {
            let tmp = snapshot_dir();
            let dir = tmp.path();
            write_raw(dir, "{\"format\":1,\"current\":\"gen-gone\"}");
            assert!(matches!(
                read_marker(dir),
                Err(NoTier::Unreadable {
                    format: Some(1),
                    ..
                })
            ));
            std::fs::write(dir.join("gen-file"), b"").expect("file");
            write_raw(dir, "{\"format\":1,\"current\":\"gen-file\"}");
            assert!(matches!(
                read_marker(dir),
                Err(NoTier::Unreadable {
                    format: Some(1),
                    ..
                })
            ));
            let outside = tempfile::tempdir().expect("outside");
            std::os::unix::fs::symlink(outside.path(), dir.join("gen-link")).expect("symlink");
            write_raw(dir, "{\"format\":1,\"current\":\"gen-link\"}");
            assert!(matches!(
                read_marker(dir),
                Err(NoTier::Unreadable {
                    format: Some(1),
                    ..
                })
            ));
        }
    }
}
