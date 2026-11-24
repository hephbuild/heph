//! `HEPH_CWD` points heph at another tree: the cwd, the workspace root walked up
//! from it, and the cwd's package under that root all follow it.
//!
//! A separate binary because it sets `HEPH_CWD` (a `set_var`), and because the
//! three are resolved once per process (`get_cwd`/`get_root`/`get_cwp` are
//! `LazyLock`s): the first call in this process must be the one that sees the
//! variable, so no other test may run in it first.

use heph::engine::{get_cwd, get_cwp, get_root};
use heph::htpkg::PkgBuf;

#[test]
fn heph_cwd_sets_the_root_and_the_cwd_package_once_per_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("ws");
    let sub = root.join("svc/api");
    std::fs::create_dir_all(&sub).expect("create the subdir");
    std::fs::write(root.join(".hephconfig"), "").expect("write the workspace config");
    // Canonical, so the comparisons below hold however the platform spells its
    // temp dir (`/var` → `/private/var` on macOS).
    let root = std::fs::canonicalize(&root).expect("canonicalize the root");
    let sub = root.join("svc/api");

    // SAFETY: this binary's only test, run before anything reads the
    // environment on another thread.
    unsafe { std::env::set_var("HEPH_CWD", &sub) };

    assert_eq!(get_cwd().expect("cwd"), sub);
    assert_eq!(
        get_root().expect("root"),
        root,
        "the root is walked up from HEPH_CWD"
    );
    assert_eq!(
        get_cwp().expect("cwp"),
        PkgBuf::from("svc/api"),
        "the cwd package is HEPH_CWD relative to the root"
    );

    // Resolved once: a later change to the variable is not seen.
    // SAFETY: as above.
    unsafe { std::env::set_var("HEPH_CWD", &root) };
    assert_eq!(get_cwd().expect("cwd"), sub);
    assert_eq!(get_root().expect("root"), root);
    assert_eq!(get_cwp().expect("cwp"), PkgBuf::from("svc/api"));
}
