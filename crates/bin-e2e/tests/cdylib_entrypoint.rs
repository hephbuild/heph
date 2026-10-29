#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! An entrypoint declared by a third-party driver in a REAL cdylib
//! (`crates/plugin-entrypoint-test-cdylib`, staged as
//! `test-entrypoint-plugin.<ext>`, never published), loaded by the shipped
//! binary through a manifest and run in a terminal.
//!
//! The rows are the in-process seam test's (`cdylib_entrypoint_runs` in
//! `crates/e2e/tests/entrypoint.rs`); what only this one crosses is the
//! `dlopen` itself — the ABI and layout checks, the cdylib's own SDK statics
//! (its record of the host's entrypoint capability), its own `proc` and
//! supervisor sink, its own runtime — and a real tty.

mod common;

use anyhow::Context as _;
use common::pty::{Ended, Pty};
use common::{BASE_CONFIG, Dist, Workspace, sha256_file, write_manifest};
use std::path::Path;
use std::time::Duration;

/// Generous: a release binary loading a plugin and running a tool on a shared
/// CI runner.
const DEADLINE: Duration = Duration::from_secs(120);

/// A workspace whose `//app:tool` is the test cdylib's driver, and a caller
/// directory below its root.
fn workspace(dist: &Dist) -> anyhow::Result<(Workspace, std::path::PathBuf)> {
    let ws = Workspace::new()?;
    let dylib = dist.test_plugin("entrypoint");
    anyhow::ensure!(dylib.is_file(), "missing {}", dylib.display());
    let manifest = ws.root().join("test-entrypoint-plugin.json");
    write_manifest(
        &manifest,
        "entrypoint_test",
        &dylib,
        Some(&sha256_file(&dylib)?),
    )?;
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))?;
    ws.write(
        "app/BUILD",
        "target(name = \"tool\", driver = \"entrypoint_test.tool\")\n",
    )?;
    let caller = ws.root().join("some/where");
    std::fs::create_dir_all(&caller)?;
    Ok((ws, caller))
}

fn read(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// `heph run //app:tool -- a 'b c'` from a directory below the root, under a
/// PTY: the cdylib's tool runs in the caller's directory with the caller's
/// environment plus `HEPH_ROOT`, gets the args byte for byte and the terminal
/// on all three fds, and its exit code is heph's. Then a SIGTERM sent to
/// heph's pid reaches the tool's trap through the run stream, exactly once,
/// and the tool's later exit code is heph's.
#[test]
fn cdylib_entrypoint_runs() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let (ws, caller) = workspace(&dist)?;
    let cwd = caller.canonicalize()?;

    // Ran to completion.
    let out = cwd.join("out");
    let out_s = out.to_string_lossy().into_owned();
    let mut pty = Pty::spawn(
        &dist,
        &caller,
        &["run", "//app:tool", "--", "a", "b c"],
        &[
            ("HEPH_TEST_CALLER", "from-caller"),
            ("HEPH_TEST_OUT", &out_s),
        ],
    );
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(42), "{}", pty.seen());
    let seen = read(&out)?;
    let root = seen
        .lines()
        .find_map(|l| l.strip_prefix("root="))
        .with_context(|| format!("no root line\n{seen}"))?;
    assert_eq!(
        Path::new(root).canonicalize()?,
        ws.root().canonicalize()?,
        "HEPH_ROOT is the workspace root\n{seen}"
    );
    assert_eq!(
        seen,
        format!(
            "cwd={}\ncaller=from-caller\nroot={root}\ntty=yes\narg=[a]\narg=[b c]\n",
            cwd.display()
        ),
    );

    // A TERM to heph's pid, relayed once; the tool decides when it ends.
    let out = cwd.join("waits");
    let out_s = out.to_string_lossy().into_owned();
    let mut pty = Pty::spawn(
        &dist,
        &caller,
        &["run", "//app:tool"],
        &[
            ("HEPH_TEST_CALLER", "from-caller"),
            ("HEPH_TEST_OUT", &out_s),
            ("HEPH_TEST_MODE", "wait"),
        ],
    );
    pty.wait_for("trap-ready", DEADLINE);
    let terms = cwd.join("waits.term");
    common::signal(pty.pid(), libc::SIGTERM);
    common::wait_until(
        "the TERM to reach the tool",
        Duration::from_secs(20),
        || terms.exists(),
    );
    // Time enough for a duplicate delivery to land.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(read(&terms)?, "got-term\n", "one TERM, one delivery");
    std::fs::write(cwd.join("waits.stop"), b"")?;
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(7), "{}", pty.seen());
    assert_eq!(read(&terms)?, "got-term\n", "one TERM, one delivery");
    assert!(pty.cursor_visible(), "cursor left hidden\n{}", pty.screen());
    Ok(())
}
