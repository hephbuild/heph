//! Revision pins: a file outside the cache that keeps something alive for as
//! long as one cached revision of a target exists. Today that is the nix
//! driver's gcroot symlinks, which stop `nix-collect-garbage` from reclaiming
//! the store paths a cached wrapper points at.
//!
//! A pin is `<dir>/<name>` plus a sidecar `<dir>/<name>.rev` naming its
//! revision: the `hashin` on the first line, the target's address after it.
//! The name is derived from the revision ([`paths`]), but lossily, so it is the
//! sidecar that lets `heph tool gc` ([`list`]) map a pin back to the revision it
//! pins and remove it once that revision is gone from the cache.
//!
//! Order matters on both sides:
//! - A driver writes the sidecar ([`write_sidecar`]) **before** it creates the
//!   pin, so a pin never exists without one while its build runs.
//! - [`Pin::remove`] deletes the pin before the sidecar, so a crash in between
//!   leaves a sidecar with nothing to pin, which the next sweep removes.
//!
//! A pin without a readable sidecar names no revision and is never removed:
//! not knowing what it pins, deleting it could release something a cached
//! revision still needs.

use anyhow::Context;
use hmodel::htaddr::{Addr, parse_addr};
use std::path::{Path, PathBuf};

/// The sidecar's extension.
const SIDECAR_EXT: &str = "rev";

/// Where one revision's pin and its sidecar live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinPaths {
    pub pin: PathBuf,
    pub sidecar: PathBuf,
}

/// The pin of revision `(addr, hashin)` in `dir`: `<addr hash>-<hashin hash>`.
/// The `hashin` is hashed rather than spliced in, so the name is a bounded,
/// filesystem-safe component whatever the key.
pub fn paths(dir: &Path, addr: &Addr, hashin: &str) -> PinPaths {
    let name = format!(
        "{}-{:x}",
        addr.hash_str(),
        xxhash_rust::xxh3::xxh3_64(hashin.as_bytes())
    );
    PinPaths {
        sidecar: dir.join(format!("{name}.{SIDECAR_EXT}")),
        pin: dir.join(name),
    }
}

/// Record which revision `paths` pins. Call before creating the pin.
///
/// Atomic: written to a temp file in the same directory, then renamed over the
/// sidecar, so gc never reads a half-written one. The temp name does not end
/// in `.rev`, so [`list`] never takes it for a sidecar.
pub async fn write_sidecar(paths: &PinPaths, addr: &Addr, hashin: &str) -> anyhow::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = paths.sidecar.clone().into_os_string();
    tmp.push(format!(".{}-{seq}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, sidecar_contents(addr, hashin))
        .await
        .with_context(|| format!("write revision pin sidecar {}", tmp.display()))?;
    if let Err(e) = tokio::fs::rename(&tmp, &paths.sidecar).await {
        // Best-effort: the rename's error is the one worth reporting.
        if let Err(rm) = tokio::fs::remove_file(&tmp).await {
            tracing::debug!(tmp = %tmp.display(), error = %rm, "removing a sidecar temp file");
        }
        return Err(e).with_context(|| {
            format!(
                "rename revision pin sidecar into {}",
                paths.sidecar.display()
            )
        });
    }
    Ok(())
}

fn sidecar_contents(addr: &Addr, hashin: &str) -> String {
    // The hashin first: it is one token with no newline, so everything after
    // the first newline is the address, whatever characters it holds.
    format!("{hashin}\n{}", addr.format())
}

fn parse_sidecar(text: &str) -> anyhow::Result<(Addr, String)> {
    let (hashin, addr) = text
        .split_once('\n')
        .context("no newline between hashin and addr")?;
    anyhow::ensure!(!hashin.is_empty(), "empty hashin");
    let addr = parse_addr(addr).with_context(|| format!("parse addr {addr:?}"))?;
    Ok((addr, hashin.to_string()))
}

/// One pin, and the revision its sidecar names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub addr: Addr,
    pub hashin: String,
    pub paths: PinPaths,
}

impl Pin {
    /// Remove the pin, then its sidecar. Either one already gone is fine.
    pub fn remove(&self) -> anyhow::Result<()> {
        for p in [&self.paths.pin, &self.paths.sidecar] {
            match std::fs::remove_file(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("remove revision pin {}", p.display()));
                }
            }
        }
        Ok(())
    }
}

/// Every pin in `dir` with a readable sidecar. A missing `dir` has none. A
/// sidecar that does not read or parse is skipped (and logged), so its pin is
/// kept: see the module doc.
pub fn list(dir: &Path) -> anyhow::Result<Vec<Pin>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("list revision pins in {}", dir.display()));
        }
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.with_context(|| format!("list revision pins in {}", dir.display()))?;
        let sidecar = entry.path();
        if sidecar.extension().is_none_or(|e| e != SIDECAR_EXT) {
            continue;
        }
        let parsed = std::fs::read_to_string(&sidecar)
            .context("read")
            .and_then(|text| parse_sidecar(&text));
        let (addr, hashin) = match parsed {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    sidecar = %sidecar.display(),
                    error = %format!("{e:#}"),
                    "revision pin sidecar unreadable, its pin is kept"
                );
                continue;
            }
        };
        let paths = paths(dir, &addr, &hashin);
        if paths.sidecar != sidecar {
            // Names a revision other than the one its file name is derived
            // from: not one of ours to judge.
            tracing::debug!(sidecar = %sidecar.display(), "revision pin sidecar does not match its name, kept");
            continue;
        }
        out.push(Pin {
            addr,
            hashin,
            paths,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> Addr {
        parse_addr(s).expect("addr")
    }

    #[test]
    fn name_is_per_revision_and_path_safe() {
        let dir = Path::new("/d");
        let a = addr("//p:a");
        assert_ne!(paths(dir, &a, "h1"), paths(dir, &a, "h2"));
        assert_ne!(paths(dir, &a, "h1"), paths(dir, &addr("//p:ab"), "h1"));
        assert_eq!(paths(dir, &a, "h1"), paths(dir, &a, "h1"));
        let p = paths(dir, &a, "../../x/y");
        assert_eq!(p.pin.parent(), Some(dir));
        assert_eq!(p.sidecar.parent(), Some(dir));
    }

    #[test]
    fn list_maps_pins_back_to_their_revision() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let a = addr("//p:a@k=v");
        let p = paths(dir, &a, "h1");
        std::fs::write(&p.sidecar, sidecar_contents(&a, "h1")).expect("sidecar");
        std::fs::write(&p.pin, "").expect("pin");
        // A pin with no sidecar, and a sidecar that does not parse.
        std::fs::write(dir.join("legacy"), "").expect("legacy pin");
        std::fs::write(dir.join("junk.rev"), "no newline").expect("junk");

        let pins = list(dir).expect("list");
        assert_eq!(
            pins,
            [Pin {
                addr: a,
                hashin: "h1".to_string(),
                paths: p.clone(),
            }]
        );
        pins[0].remove().expect("remove");
        assert!(!p.pin.exists() && !p.sidecar.exists());
        assert!(dir.join("legacy").exists(), "an unnamed pin is never swept");
        pins[0].remove().expect("removing twice is fine");
    }

    #[test]
    fn list_of_a_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(list(&tmp.path().join("nope")).expect("list").is_empty());
    }
}
