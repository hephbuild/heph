//! The engine's two checks on the host paths a run declares
//! (`hplugin::driver::HostPath`): when the entry is written ([`checked_for_entry`],
//! I6 and I8) and on every local hit ([`liveness`], I3 and I4). Neither reaches
//! any hash (I1).

use anyhow::Context;
use hmodel::htaddr::Addr;
use hplugin::driver::HostPath;

/// Whether a local entry's declared host paths all still exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Liveness {
    Live,
    /// The first declared path found gone. The entry is not served.
    Stale {
        path: String,
    },
}

/// The list a run declared, as the entry stores it, or the error that fails
/// the run before anything is written.
///
/// - A target with `cache.remote` on may declare none (I6, Decision 14): its
///   entry would be uploaded, and another machine has no such path. This reads
///   the target's own parse-time flag, never whether a remote is configured,
///   so a target fails the same way on a laptop and on CI.
/// - The list is deduplicated, sorted and capped at
///   [`MAX_HOST_PATHS`](hplugin::driver::MAX_HOST_PATHS) (I8). Each path's
///   shape was checked when it was constructed.
/// - Every path exists now (I8), checked off the async worker: a mistyped path
///   would otherwise make the entry stale on every run.
///
/// The caller skips this for a run that writes no durable entry (caching off,
/// a shell): the list is ignored there.
pub(crate) async fn checked_for_entry(
    addr: &Addr,
    remote_enabled: bool,
    mut paths: Vec<HostPath>,
) -> anyhow::Result<Vec<HostPath>> {
    if paths.is_empty() {
        return Ok(paths);
    }
    if remote_enabled {
        paths.sort();
        paths.dedup();
        anyhow::bail!(
            "{addr} declares host paths ({}) with `cache.remote` on: an entry that names host \
             paths must not leave this machine. Set `cache.remote = false` on the target",
            join(&paths)
        );
    }
    HostPath::normalize_list(&mut paths)
        .with_context(|| format!("checking the host paths {addr} declares"))?;
    let addr = addr.clone();
    hcore::blocking::run(move || {
        for path in &paths {
            if let Err(e) = std::fs::metadata(path.as_path()) {
                let what = if is_gone(&e) {
                    "which does not exist"
                } else {
                    "which cannot be checked"
                };
                return Err(e).with_context(|| {
                    format!("{addr} declares host path {path}, {what} when its entry is written")
                });
            }
        }
        Ok(paths)
    })
    .await
}

/// Whether every one of `paths` still exists: one `stat` each, which follows
/// symlinks, so a dangling link is gone. Runs inside the manifest read's
/// blocking job, never on a tokio worker (I4).
///
/// `ENOENT` and `ENOTDIR` are gone. Any other error (`EACCES`, `EIO`) fails the
/// target rather than calling the entry stale: a path the engine cannot stat
/// would otherwise rebuild on every run.
pub(crate) fn liveness(addr: &Addr, paths: &[String]) -> anyhow::Result<Liveness> {
    for path in paths {
        match std::fs::metadata(path) {
            Ok(_) => {}
            Err(e) if is_gone(&e) => {
                return Ok(Liveness::Stale { path: path.clone() });
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("checking host path {path}, which the cache entry of {addr} declares")
                });
            }
        }
    }
    Ok(Liveness::Live)
}

/// The reason a stale entry's miss reports (`CacheOutcome.reason`).
pub(crate) fn stale_reason(path: &str) -> String {
    format!("stale: host path {path} is gone")
}

/// `ENOENT` or `ENOTDIR`: the path is gone, as opposed to unreadable.
fn is_gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn join(paths: &[HostPath]) -> String {
    paths
        .iter()
        .map(HostPath::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> Addr {
        Addr::new(
            hmodel::htpkg::PkgBuf::from("p"),
            "t".to_string(),
            Default::default(),
        )
    }

    fn s(p: &std::path::Path) -> String {
        p.to_str().expect("utf-8 tempdir").to_owned()
    }

    #[test]
    fn liveness_follows_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f");
        std::fs::write(&file, b"").expect("write");
        let good = dir.path().join("good");
        std::os::unix::fs::symlink(&file, &good).expect("symlink");
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("nothing"), &dangling).expect("symlink");
        let under_file = file.join("x");

        assert_eq!(liveness(&addr(), &[]).unwrap(), Liveness::Live);
        assert_eq!(
            liveness(&addr(), &[s(&file), s(&good)]).unwrap(),
            Liveness::Live
        );
        for gone in [dangling, under_file, dir.path().join("missing")] {
            assert_eq!(
                liveness(&addr(), &[s(&file), s(&gone)]).unwrap(),
                Liveness::Stale { path: s(&gone) },
                "{}",
                gone.display()
            );
        }
    }
}
