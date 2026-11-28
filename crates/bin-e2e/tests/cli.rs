//! Process-level guarantees of the shipped binary: that it launches at all on
//! this host, and that a run's outcome reaches the shell as an exit status.
//!
//! None of this is reachable in-process — a linked test observes an
//! `anyhow::Result`, never an exit code, and it exercises the *test* build's
//! link closure rather than the artifact CI publishes.

mod common;

use common::session::{Bg, SHORT, recorded};
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

/// `slow` runs until it is stopped; `bad` fails at once.
const CANCEL_BUILD: &str = r#"
target(name = "slow", driver = "bash", run = "sleep 60", cache = False)
target(name = "bad", driver = "bash", run = "exit 3", cache = False)
"#;

/// `heph run <args>` with no terminal (the line backend), stopped by `sig`
/// once `//pkg:slow` runs and, first, once each of `after` has ended: the
/// exit code and stderr.
fn stopped_run(args: &[&str], after: &[&str], sig: i32) -> (Option<i32>, String) {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write("pkg/BUILD", CANCEL_BUILD).expect("write BUILD");
    let mut run = Bg::spawn(&ws, &dist, &[&["run"], args].concat());
    run.wait_executing("//pkg:slow");
    for addr in after {
        common::wait_until(&format!("{addr} to end"), SHORT, || {
            recorded(&run.dir, "ResultEnd", addr)
        });
    }
    common::signal(run.pid(), sig);
    let status = run.wait();
    (status.code(), run.stderr())
}

/// A Ctrl-C (SIGINT) with no terminal is a cancel, and a cancel exits 130
/// (error-reporting I6, RUN-22), not 1.
#[test]
fn cancel_exits_130_non_tui() {
    let (code, stderr) = stopped_run(&["//pkg:slow"], &[], libc::SIGINT);
    assert_eq!(code, Some(130), "{stderr}");
}

/// A target failed, then the user stopped the rest: the stop decides the exit
/// code (I6), while the failure's box is still printed.
#[test]
fn failure_then_ctrl_c_exits_130() {
    let (code, stderr) = stopped_run(
        &["-e", "//pkg:slow || //pkg:bad"],
        &["//pkg:bad"],
        libc::SIGINT,
    );
    assert_eq!(code, Some(130), "{stderr}");
    assert!(
        stderr.contains("//pkg:bad"),
        "the failure is reported\n{stderr}"
    );
}

/// RUN-24: under `--fail-fast`, `bad`'s failure stops its sibling `slow`.
/// That stop is heph's, not the user's, so the run exits 1, never 130,
/// whether `slow` was running, queued or not yet started when `bad` failed.
/// Which one it is depends on scheduling, so the run is repeated.
#[test]
fn fail_fast_after_failure_exits_1() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write("pkg/BUILD", CANCEL_BUILD).expect("write BUILD");
    for i in 0..20 {
        let out = ws
            .run(
                &dist,
                &["run", "--fail-fast", "-e", "//pkg:slow || //pkg:bad"],
            )
            .expect("run");
        assert_eq!(out.status.code(), Some(1), "run {i}: {}", describe(&out));
    }
}

/// SIGTERM is a cancel, as Ctrl-C is: heph stops the build and exits 130,
/// rather than dying of the signal.
#[test]
fn sigterm_exits_130() {
    let (code, stderr) = stopped_run(&["//pkg:slow"], &[], libc::SIGTERM);
    assert_eq!(code, Some(130), "{stderr}");
}

/// SIGHUP (the terminal went away) is a cancel too: exit 130.
#[test]
fn sighup_exits_130() {
    let (code, stderr) = stopped_run(&["//pkg:slow"], &[], libc::SIGHUP);
    assert_eq!(code, Some(130), "{stderr}");
}

/// Started with SIGHUP ignored, as `nohup` starts it, heph leaves it ignored:
/// a HUP mid-build changes nothing, and the run finishes and exits 0, not
/// 130. TERM is still a cancel then (heph only keeps the ignore it was given).
#[test]
fn sighup_ignored_when_inherited_ignored() {
    use std::os::unix::process::CommandExt as _;

    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"nap\", driver = \"bash\", run = \"sleep 3\", cache = False)\n",
    )
    .expect("write BUILD");
    let mut run = Bg::spawn_with(&ws, &dist, &["run", "//pkg:nap"], |cmd| {
        let ignore_hup = || {
            // SAFETY: setting a signal's disposition to `SIG_IGN`.
            unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
            Ok(())
        };
        // SAFETY: the closure only calls `signal(2)`, which is
        // async-signal-safe, between fork and exec.
        unsafe { cmd.pre_exec(ignore_hup) };
    });
    run.wait_executing("//pkg:nap");
    common::signal(run.pid(), libc::SIGHUP);
    let status = run.wait();
    assert_eq!(status.code(), Some(0), "{}", run.stderr());
}
