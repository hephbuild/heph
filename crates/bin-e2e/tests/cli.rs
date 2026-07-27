//! Process-level guarantees of the shipped binary: that it launches at all on
//! this host, and that a run's outcome reaches the shell as an exit status.
//!
//! None of this is reachable in-process — a linked test observes an
//! `anyhow::Result`, never an exit code, and it exercises the *test* build's
//! link closure rather than the artifact CI publishes.

mod common;

use common::{Dist, Workspace, describe};

/// The published artifact must launch on a stock host. This is the canary for
/// the whole class of link-time portability breakage that only shows at
/// `execve` time: the macOS `libiconv` /nix/store hard-link (dyld aborts),
/// weak-linked libfuse with no macFUSE installed, and a glibc floor raised
/// above the oldest supported distro. Every one of those passes `cargo test`
/// and fails here.
///
/// `version` is the right probe: it is the one command that runs outside a
/// workspace, so a failure is unambiguously about the binary, not the fixture.
#[test]
fn shipped_binary_launches_outside_a_workspace() {
    let dist = Dist::locate();
    let home = tempfile::tempdir().expect("home tempdir");
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    let out = std::process::Command::new(dist.heph())
        .arg("version")
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .output()
        .expect("spawn heph version");

    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "version printed nothing: {}",
        describe(&out)
    );
}

/// A target that exits non-zero must make `heph` exit non-zero. The mapping
/// from a failed build to `ExitCode` lives in `main`, past the last function an
/// in-process test can call.
#[test]
fn failing_target_exits_nonzero() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"boom\", driver = \"bash\", run = \"exit 3\", cache = False)\n",
    )
    .expect("write BUILD");

    let out = ws.run(&dist, &["run", "//pkg:boom"]).expect("run");

    assert!(
        !out.status.success(),
        "a failing target must not exit 0: {}",
        describe(&out)
    );
}

/// The inverse, so the previous test can't pass because the fixture is broken:
/// the same shape of target succeeding exits 0.
#[test]
fn succeeding_target_exits_zero() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"ok\", driver = \"bash\", run = \"echo e2e-ok\", cache = False)\n",
    )
    .expect("write BUILD");

    let out = ws.run(&dist, &["run", "//pkg:ok"]).expect("run");

    assert!(out.status.success(), "{}", describe(&out));
}

/// With stderr piped rather than attached to a terminal, the interactive
/// renderer must stay off. A tool that drives a viewport when its output is
/// being captured corrupts every CI log and every `heph … | tee` — and the tty
/// check that prevents it is unobservable from a linked test, which has no
/// controlling terminal either way and so passes vacuously.
#[test]
fn piped_output_stays_plain() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"ok\", driver = \"bash\", run = \"echo e2e-ok\", cache = False)\n",
    )
    .expect("write BUILD");

    let out = ws.run(&dist, &["run", "//pkg:ok"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));

    // The two things only the interactive backend does: ask the terminal where
    // the cursor is (to place its inline viewport) and take the cursor. The PTY
    // suite asserts both are present when a tty *is* attached.
    for (name, seq) in [
        ("cursor-position query", b"\x1b[6n".as_slice()),
        ("cursor hide", b"\x1b[?25l".as_slice()),
    ] {
        assert!(
            !contains(&out.stderr, seq) && !contains(&out.stdout, seq),
            "emitted a {name} with no tty attached: {}",
            describe(&out)
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
