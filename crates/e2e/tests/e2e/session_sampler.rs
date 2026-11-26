//! The machine sampler starts with the recorder, and only with it: `--raw`
//! records nothing and so samples nothing, while a private session
//! (`HEPH_NO_SESSION`) records, and samples, like any other.
//!
//! `--raw` is decided in `src/main.rs`, which configures the session slot
//! with `Ok(None)`; the test makes the same call, since `main` is not
//! reachable in-process. The spawn counter is per thread, so the other tests
//! of this binary cannot move it.

use heph::tui::session::sampler::spawned_on_this_thread;
use heph::tui::session::{HeaderKind, SessionHeader, SessionRoot, SessionSlot};

fn header() -> SessionHeader {
    SessionHeader {
        kind: HeaderKind::Build,
        label: "run //p:t".into(),
    }
}

fn argv() -> Vec<String> {
    vec!["heph".into(), "run".into(), "//p:t".into()]
}

/// Spec row `raw_starts_no_sampler`.
#[test]
fn raw_starts_no_sampler() {
    let home = tempfile::tempdir().expect("tempdir");

    // What `main` does for `--raw`.
    let before = spawned_on_this_thread();
    let raw = SessionSlot::new();
    raw.configure(Ok(None), argv());
    assert!(
        raw.open(header(), false).is_none(),
        "--raw opens no session"
    );
    raw.finish(Some(0));
    assert_eq!(spawned_on_this_thread(), before, "--raw starts no sampler");

    // `HEPH_NO_SESSION=1`: a private session, which samples.
    let root = SessionRoot::from_env(Some("1".as_ref()), Some(home.path().as_os_str()))
        .expect("a root")
        .expect("a private root with HOME set");
    assert!(root.is_private());
    let private = SessionSlot::new();
    private.configure(Ok(Some(root)), argv());
    let opened = private
        .open(header(), false)
        .expect("a private session opens");
    assert!(opened.private);
    assert_eq!(
        spawned_on_this_thread(),
        before + 1,
        "a private session starts one sampler"
    );
    private.finish(Some(0));
}
