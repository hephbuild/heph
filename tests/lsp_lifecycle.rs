//! The BUILD-file language server (`heph tool build-lsp`) must terminate when the
//! editor ends the session — both on the LSP `exit` notification and on a plain
//! stdin close (disconnect). Regression test for a hang where the server blocked
//! on `io_threads.join()` (its stdin reader never ending), so the editor SIGKILLed
//! it and "restart language server" failed.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

/// A minimal heph workspace (buildfile provider + exec driver).
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(".hephconfig2"),
        "providers:\n  - name: buildfile\ndrivers:\n  - name: exec\n",
    )
    .expect("write config");
    dir
}

fn frame(msg: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg).into_bytes()
}

/// Wait up to `timeout` for the child to exit; return whether it did.
fn exited_within(child: &mut std::process::Child, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    false
}

fn spawn_lsp(ws: &tempfile::TempDir) -> std::process::Child {
    let mut child = Command::new(heph_bin())
        .args(["tool", "build-lsp"])
        .current_dir(ws.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn build-lsp");
    // Drain stdout so the server never blocks writing responses.
    let stdout = child.stdout.take().expect("stdout");
    std::thread::spawn(move || {
        use std::io::Read;
        let mut sink = Vec::new();
        let _n = std::io::BufReader::new(stdout).read_to_end(&mut sink);
    });
    child
}

#[test]
fn build_lsp_exits_on_exit_notification() {
    let ws = workspace();
    let mut child = spawn_lsp(&ws);
    let root = format!("file://{}", ws.path().display());
    {
        let mut stdin = child.stdin.take().expect("stdin");
        let init = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{}},"rootUri":"{root}"}}}}"#
        );
        stdin.write_all(&frame(&init)).unwrap();
        stdin
            .write_all(&frame(
                r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            ))
            .unwrap();
        stdin
            .write_all(&frame(
                r#"{"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}"#,
            ))
            .unwrap();
        stdin
            .write_all(&frame(r#"{"jsonrpc":"2.0","method":"exit","params":null}"#))
            .unwrap();
        stdin.flush().unwrap();
        // stdin stays open (dropped at scope end) — the server must exit on the
        // `exit` notification alone, not because stdin closed.
    }

    assert!(
        exited_within(&mut child, Duration::from_secs(15)),
        "server did not exit on the `exit` notification"
    );
}

#[test]
fn build_lsp_exits_on_stdin_close() {
    let ws = workspace();
    let mut child = spawn_lsp(&ws);
    let root = format!("file://{}", ws.path().display());
    let init = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{}},"rootUri":"{root}"}}}}"#
    );
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(&frame(&init)).unwrap();
        stdin.flush().unwrap();
        // Drop stdin → client disconnect without the shutdown/exit handshake.
    }
    assert!(
        exited_within(&mut child, Duration::from_secs(15)),
        "server did not exit when stdin closed"
    );
}
