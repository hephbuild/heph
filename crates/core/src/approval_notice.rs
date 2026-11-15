//! Approval notices stored by content.
//!
//! At gate time the engine writes each rendered notice to
//! `<home>/approval/notices/<hash>`, `hash` being the xxh3-128 of its bytes,
//! and the session records that hash beside the notice. A reader — the live
//! approval view or an attached one — reads it back through
//! [`read_notice_blob`], which checks the hash: a blob that is gone or does
//! not match reads as [`NOTICE_UNAVAILABLE`], never as a path to go and open.
//!
//! **Diagnostics only.** A notice is shown to a person deciding an approval;
//! nothing here reaches a cache key, a def hash, a sandbox, or a target's env,
//! argv or files.

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The directory under the heph home that holds notice blobs.
pub const NOTICES_DIR: &str = "approval/notices";

/// What a reader shows in place of a notice it cannot read back: its blob is
/// gone, does not match its hash, or the session predates notice hashes.
pub const NOTICE_UNAVAILABLE: &str = "notice unavailable";

/// The content hash a notice is stored under: xxh3-128 of its bytes, 32
/// lowercase hex digits.
pub fn notice_hash(content: &[u8]) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(content))
}

/// Whether `hash` has the shape [`notice_hash`] produces, so joining it under
/// [`NOTICES_DIR`] cannot leave that directory.
fn is_notice_hash(hash: &str) -> bool {
    hash.len() == 32
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn blob_path(home: &Path, hash: &str) -> PathBuf {
    home.join(NOTICES_DIR).join(hash)
}

/// Write `content` under its content hash in `home` and return the hash.
/// Blocking. The blob is written to a temporary name and renamed into place,
/// so a reader never sees half of it; a blob already there is replaced by the
/// same bytes.
pub fn write_notice_blob(home: &Path, content: &str) -> anyhow::Result<String> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let hash = notice_hash(content.as_bytes());
    let dir = home.join(NOTICES_DIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating approval notice dir {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{hash}.{}.{}.tmp",
        std::process::id(),
        // A counter, not ordering: Relaxed only keeps two writers in one
        // process off the same temporary name.
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, content)
        .with_context(|| format!("writing approval notice {}", tmp.display()))?;
    let dst = blob_path(home, &hash);
    if let Err(e) = std::fs::rename(&tmp, &dst) {
        // Best effort: the rename's error is the one worth reporting.
        let _removed = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("storing approval notice {}", dst.display()));
    }
    Ok(hash)
}

/// Read the notice stored under `hash` in `home`, checking it still hashes to
/// `hash`. Blocking (one file read): call it off the render task. `None` —
/// show [`NOTICE_UNAVAILABLE`] — when `hash` is absent (a session from a heph
/// that predates notice hashes), malformed, its blob is gone or unreadable, or
/// the bytes no longer match.
pub fn read_notice_blob(home: &Path, hash: Option<&str>) -> Option<String> {
    let hash = hash.filter(|h| is_notice_hash(h))?;
    let bytes = std::fs::read(blob_path(home, hash)).ok()?;
    if notice_hash(&bytes) != hash {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_notice_reads_back_by_its_hash() {
        let home = tempfile::tempdir().expect("tempdir");
        let hash = write_notice_blob(home.path(), "push api:prod\n").expect("write");
        assert_eq!(hash, notice_hash(b"push api:prod\n"));
        assert_eq!(
            read_notice_blob(home.path(), Some(&hash)).as_deref(),
            Some("push api:prod\n")
        );
        // Same content, same blob: a second write replaces it in place.
        assert_eq!(
            write_notice_blob(home.path(), "push api:prod\n").expect("rewrite"),
            hash
        );
        let names: Vec<_> = std::fs::read_dir(home.path().join(NOTICES_DIR))
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from(&hash)],
            "no temporary left behind"
        );
    }

    #[test]
    fn notice_unavailable_never_a_path() {
        let home = tempfile::tempdir().expect("tempdir");
        let hash = write_notice_blob(home.path(), "the plan").expect("write");

        // An older session: no hash recorded.
        assert_eq!(read_notice_blob(home.path(), None), None);
        // A hash that does not name a blob, or tries to leave the directory.
        for bad in [
            "",
            "../../etc/passwd",
            &hash.to_uppercase(),
            &format!("{hash}0"),
        ] {
            assert_eq!(read_notice_blob(home.path(), Some(bad)), None, "{bad:?}");
        }
        // Bytes that no longer match their hash.
        std::fs::write(home.path().join(NOTICES_DIR).join(&hash), "another plan").expect("tamper");
        assert_eq!(read_notice_blob(home.path(), Some(&hash)), None);
        // The blob is gone.
        std::fs::remove_file(home.path().join(NOTICES_DIR).join(&hash)).expect("remove");
        assert_eq!(read_notice_blob(home.path(), Some(&hash)), None);
    }
}
