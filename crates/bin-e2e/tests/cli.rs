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
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "version printed nothing: {}",
        describe(&out)
    );

    // `dist/heph` is always the "std" release flavour (the `e2e` script only
    // ever stages the exact `heph_<os>_<arch>` name, never a flavoured one —
    // see devenv.nix). Its `version_flavour` slot must have been patched
    // empty by `scripts/patch-slot.sh`, not left as "debug" or the
    // unpatched marker — a compile-only (in-process) test can't catch this,
    // since it never runs the patch/strip pipeline the shipped binary went
    // through. The baked-in version already carries its own build metadata (a
    // commit hash), so check for the debug flavour marker specifically —
    // `hcore::version::reported` would join it onto the existing metadata
    // with a `.`, or add a bare `+` if there wasn't any.
    let trimmed = stdout.trim();
    assert!(
        !trimmed.ends_with("+debug") && !trimmed.ends_with(".debug"),
        "std flavour must not report the debug flavour: {}",
        describe(&out)
    );

    // The version slot took as well. Same reason this test exists for the
    // flavour, and now load-bearing for the version too: it is no longer
    // compiled in, so a `patch-slot.sh` that was dropped from the pipeline, or
    // that matched nothing, yields an artifact which builds, launches and
    // prints a version — just the dev sentinel. An in-process test cannot see
    // it, because it never runs the patch/strip pipeline at all. A shipped
    // binary reporting `v0.0.0-dev` silently never self-upgrades, and makes the
    // go plugin resolve a govet target address no release publishes.
    assert!(
        !trimmed.contains(hcore_dev_version()),
        "shipped binary reports the unstamped dev version — the version slot \
         was never patched: {}",
        describe(&out)
    );
}

/// `hcore::version::DEV_VERSION`, spelled out rather than imported: `bin-e2e`
/// deliberately links no workspace crate, so that it tests the shipped
/// artifacts rather than this source tree (see `.claude/testing.md`).
fn hcore_dev_version() -> &'static str {
    "v0.0.0-dev"
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
