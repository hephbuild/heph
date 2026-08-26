//! Agent-mode exec runners, through the shipped binary.
//!
//! `crates/execrunner`'s own suite covers the agent *protocol* — the
//! `SCM_RIGHTS` handoff, `dup2` onto the target's stdio, exit and signal
//! fidelity, a target dying when its client disconnects. What it structurally
//! cannot cover is everything between "a `runner.json` says `session`" and "an
//! agent is running", because that path goes through the heph *binary* twice:
//!
//! - `SessionRunner` launches `<current_exe> __runner-agent --socket S`. In an
//!   in-process test `current_exe()` is the libtest harness, whose `main` never
//!   dispatches the subcommand, so the agent never starts.
//! - Each target is spawned as `<current_exe> __runner-exec -- <program> …`,
//!   dispatched by a raw `args_os` scan before clap. Same problem.
//!
//! So the protocol could be perfect and the wiring entirely broken with every
//! other test green. That is what this file is for.
//!
//! No devenv or docker here on purpose: the *runner* is what is under test, and
//! `/bin/sh -c 'exec "$@"' sh` is a legitimate environment — one that adds
//! nothing. A dependency on a nix toolchain would make this a test of the
//! developer's machine.

mod common;

use common::{Dist, Workspace, describe};

/// A runner target emitting a `session` `runner.json` whose "environment" is a
/// passthrough.
///
/// `/usr/bin/env` runs its arguments as a command, so the agent ends up a
/// direct child — the same shape `devenv shell --` produces, with no devenv and
/// no shell quoting. (A `sh -c 'exec "$@"'` launch would need escaped quotes,
/// and Starlark eats backslash escapes inside a triple-quoted string, which
/// silently produces malformed JSON.)
fn passthrough_runner(name: &str) -> String {
    format!(
        r#"
target(
    name = "{name}",
    driver = "bash",
    run = """cat > $OUT <<'JSON'
{{"version": 1,
 "fingerprint": "e2e-passthrough-v1",
 "runner": "session",
 "config": {{"launch": ["/usr/bin/env"]}}}}
JSON""",
    out = "runner.json",
)
"#
    )
}

/// The whole path, once: a target names a session runner, an agent starts, the
/// target runs inside it, and its command really executes.
///
/// If any link is broken — the subcommand dispatch, the launch argv, the
/// handshake, the descriptor handoff — this fails and the unit suites do not.
///
/// The proof is a marker file at an absolute path rather than the target's
/// declared output: an output is collected into the cache and the sandbox is
/// deleted, so reading it back would test the cache. A marker tests that the
/// command ran.
#[test]
fn a_target_runs_under_a_session_runner() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let marker = ws.root().join("marker.txt");

    let mut build = passthrough_runner("runner");
    build.push_str(&format!(
        r#"
target(
    name = "hello",
    driver = "bash",
    run = "echo ran-under-the-agent > {}",
    out = [],
    runner = "//xr:runner",
)
"#,
        marker.display()
    ));
    ws.write("xr/BUILD", &build).expect("write BUILD");

    let out = ws.run(&dist, &["run", "//xr:hello"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));

    let produced = std::fs::read_to_string(&marker).unwrap_or_else(|e| {
        panic!(
            "the target never ran inside the session ({e})\n{}",
            describe(&out)
        )
    });
    assert!(
        produced.contains("ran-under-the-agent"),
        "got {produced:?}\n{}",
        describe(&out)
    );
}

/// The exit status has to survive two process hops — target → agent → client →
/// heph — and arrive as the target's own. A path that swallowed it would make
/// every failing target look like it passed.
#[test]
fn a_failing_target_under_a_session_runner_fails_the_build() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");

    let mut build = passthrough_runner("runner");
    build.push_str(
        r#"
target(
    name = "boom",
    driver = "bash",
    run = "exit 42",
    out = [],
    runner = "//xr:runner",
)
"#,
    );
    ws.write("xr/BUILD", &build).expect("write BUILD");

    let out = ws.run(&dist, &["run", "//xr:boom"]).expect("run");
    assert!(
        !out.status.success(),
        "a target that exits 42 inside the session must fail the build\n{}",
        describe(&out)
    );
}

/// One session, many targets. The pool is keyed on (address, fingerprint), so
/// two targets naming one runner share an agent rather than starting two.
///
/// Driven through a third target that depends on both, so they are built in one
/// heph process — two separate `heph run`s would prove nothing about sharing.
#[test]
fn two_targets_share_one_session() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");

    let mut build = passthrough_runner("runner");
    for name in ["a", "b"] {
        build.push_str(&format!(
            r#"
target(
    name = "{name}",
    driver = "bash",
    run = "echo from-{name} > {marker}",
    out = [],
    runner = "//xr:runner",
)
"#,
            marker = ws.root().join(format!("{name}.txt")).display()
        ));
    }
    build.push_str(
        r#"
target(
    name = "both",
    driver = "bash",
    run = "true",
    out = [],
    deps = ["//xr:a", "//xr:b"],
)
"#,
    );
    ws.write("xr/BUILD", &build).expect("write BUILD");

    let out = ws.run(&dist, &["run", "//xr:both"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));

    for name in ["a", "b"] {
        let got = std::fs::read_to_string(ws.root().join(format!("{name}.txt")))
            .unwrap_or_else(|e| panic!("{name} never ran ({e})\n{}", describe(&out)));
        assert!(got.contains(&format!("from-{name}")), "{name} got {got:?}");
    }
}

/// The session agent must not outlive the heph process that started it.
///
/// This is the one that cost a 30-minute CI timeout. The agent used to inherit
/// heph's stdout and stderr, and the registry that owns the session pool is
/// reachable from a process-global — which Rust never drops — so nothing killed
/// the agent at exit. Anything reading heph's output to EOF then waited on the
/// *agent* rather than on heph, forever, with no error and no obvious culprit.
///
/// Two things are asserted because either alone would have passed while the bug
/// was live on the other: that the run returns at all (the EOF half), and that
/// no agent is left behind (the leak half).
#[test]
fn a_session_agent_does_not_outlive_the_run() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let marker = ws.root().join("ran.txt");

    let mut build = passthrough_runner("runner");
    build.push_str(&format!(
        r#"
target(
    name = "hello",
    driver = "bash",
    run = "echo ran > {}",
    out = [],
    runner = "//xr:runner",
)
"#,
        marker.display()
    ));
    ws.write("xr/BUILD", &build).expect("write BUILD");

    // Returning at all is half the assertion: `run` reads both streams to EOF,
    // which an agent holding an inherited descriptor would never allow.
    let out = ws.run(&dist, &["run", "//xr:hello"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));

    // The socket path carries this workspace's home, so it identifies agents
    // this test started and nobody else's.
    let home = ws.root().display().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let ps = std::process::Command::new("ps")
            .args(["ax", "-o", "args="])
            .output()
            .expect("ps");
        let listing = String::from_utf8_lossy(&ps.stdout);
        let leaked: Vec<&str> = listing
            .lines()
            .filter(|l| l.contains("__runner-agent") && l.contains(&home))
            .collect();
        if leaked.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the session agent outlived the run — it will hold heph's descriptors \
             and hang whatever reads them:\n  {}",
            leaked.join("\n  ")
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Run by hand, `__runner-exec` must explain itself rather than panicking or
/// hanging on a socket nobody is listening to. It appears in every `ps` output
/// of every agent build, so someone will paste it into a shell.
#[test]
fn the_client_subcommand_run_by_hand_explains_itself() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");

    let out = ws
        .run(&dist, &["__runner-exec", "--", "/bin/echo", "hi"])
        .expect("run");
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("HEPH_RUNNER_SOCK"),
        "must name the missing variable\n{}",
        describe(&out)
    );
    assert!(
        stderr.contains("internal subcommand"),
        "must say it is not meant to be run by hand\n{}",
        describe(&out)
    );
}

/// A runner naming an implementation nobody registered must fail before the
/// target runs, and say which names exist. Covered in `crates/e2e` against the
/// engine; repeated here because the registry the *binary* assembles is a
/// different one — it includes whatever the loaded plugins contributed.
#[test]
fn an_unknown_runner_name_fails_the_build_by_name() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");

    ws.write(
        "xr/BUILD",
        r#"
target(
    name = "runner",
    driver = "bash",
    run = """cat > $OUT <<'JSON'
{"version": 1, "fingerprint": "fp", "runner": "nope", "config": {}}
JSON""",
    out = "runner.json",
)
target(
    name = "hello",
    driver = "bash",
    run = "echo hi > $OUT",
    out = "out.txt",
    runner = "//xr:runner",
)
"#,
    )
    .expect("write BUILD");

    let out = ws.run(&dist, &["run", "//xr:hello"]).expect("run");
    assert!(!out.status.success(), "{}", describe(&out));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("nope"),
        "must name the unknown runner\n{}",
        describe(&out)
    );
    assert!(
        all.contains("session") && all.contains("wrap"),
        "must list the registered runners\n{}",
        describe(&out)
    );
}
