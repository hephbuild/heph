//! What a non-tty `heph run` prints for a failure, through the built binary:
//! a target failure once, as it fails, with the rows the end-of-run box
//! would have carried; and the full box at the end only for a failure that
//! printed nothing live. The rendering rules are unit tests in
//! `src/commands/errors.rs` and `crates/tui/src/tui/progress.rs`; this is
//! the two halves agreeing on which failures went out live.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// `//p:bad` fails with a log, `//p:down` fails because it does, and
/// `//p:nodep` fails before it runs: its dep does not exist.
const BUILD: &str = r#"target(name = "bad", driver = "bash", out = "b.txt", run = "echo first; echo last words; exit 3")
target(name = "down", driver = "bash", deps = [":bad"], out = "d.txt", run = "printf x > d.txt")
target(name = "nodep", driver = "bash", deps = ["//p:nope"], out = "n.txt", run = "printf x > n.txt")
"#;

/// A workspace with its own `HOME`, on the `bash` driver.
struct Ws {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Ws {
    fn new() -> Self {
        let ws = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            home: tempfile::tempdir().expect("home"),
        };
        std::fs::write(
            ws.root().join(".hephconfig"),
            "plugins:\n  - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  - builtin: exec\n  - builtin: bash\n",
        )
        .expect("write config");
        std::fs::create_dir_all(ws.root().join("p")).expect("mkdir p");
        std::fs::write(ws.root().join("p/BUILD"), BUILD).expect("write BUILD");
        ws
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// `heph run <addr>` with stdout and stderr piped: the line backend.
    fn run(&self, addr: &str) -> (Output, String) {
        let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_heph")))
            .args(["run", addr])
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("HEPH_CWD", self.root())
            .env("HEPH_NO_SELF_UPDATE", "1")
            .env("HEPH_DISABLE_TELEMETRY", "1")
            .env_remove("GITHUB_ACTIONS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run heph");
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        (out, err)
    }
}

/// A target failure is printed once, live, ending with `full log:` and
/// `heph log`; the end of the run re-states none of it, and the run still
/// exits non-zero. A target it took down adds no second copy of the root's
/// block.
#[test]
fn ci_run_prints_a_target_failure_once() {
    let ws = Ws::new();
    for addr in ["//p:bad", "//p:down"] {
        let (out, err) = ws.run(addr);
        assert_eq!(out.status.code(), Some(1), "{addr}: {err}");
        for once in [
            "✗ //p:bad · exit status: 3\n",
            "  │ last words\n",
            "  full log: ",
            "  log: heph log //p:bad\n",
        ] {
            assert_eq!(err.matches(once).count(), 1, "{addr}: {once:?}\n{err}");
        }
        let live = err.find("✗ //p:bad").expect("the live block");
        assert!(
            live < err.find("  log: heph log //p:bad").expect("log row"),
            "{addr}: the rows are the block's\n{err}"
        );
        assert!(!err.contains("target failed"), "{addr}: no end box\n{err}");
        assert!(!err.contains("╭─[log]"), "{addr}: no framed tail\n{err}");
    }
}

/// A failure with no live block — here a dep that does not exist, so the
/// target never ran — still gets the full box at the end, and a target
/// taken down by another still gets its line in the summary.
#[test]
fn ci_run_boxes_a_failure_that_printed_nothing_live() {
    let ws = Ws::new();
    let (out, err) = ws.run("//p:nodep");
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("× target failed: //p:nodep\n╰─▶ ") && err.contains("target not found"),
        "{err}"
    );
    assert!(!err.contains("✗ //p:nodep"), "nothing live: {err}");

    let (out, err) = ws.run("//p:down");
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("✗ //p:down\n  │ meta: dependency failed (root: //p:bad)"),
        "{err}"
    );
}
