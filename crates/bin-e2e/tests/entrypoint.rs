#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `exec.entrypoint` through the shipped binary: what only a real process can
//! show. The program's exit status becomes heph's, a signal death is re-raised
//! rather than mapped to `128 + n`, and stdin reaches the program untouched.
//!
//! Everything about the closure, caching and refusals is in-process, in
//! `crates/e2e/tests/entrypoint.rs`.

mod common;

use common::{Dist, Workspace, describe};
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::Stdio;

const BUILD: &str = r#"
target(name = "code", driver = "exec.entrypoint",
       run = ["/bin/sh", "-c", "exit \"$1\"", "sh"], args = True)
target(name = "cat", driver = "exec.entrypoint", run = ["/bin/cat"])
target(name = "die", driver = "exec.entrypoint",
       run = ["/bin/sh", "-c", "ulimit -c 0; kill -s \"$1\" $$", "sh"], args = True)
"#;

fn workspace() -> anyhow::Result<Workspace> {
    let ws = Workspace::new()?;
    ws.write("app/BUILD", BUILD)?;
    Ok(ws)
}

/// The program's exit code is heph's, unchanged.
#[test]
fn entrypoint_exit_code_passes_through() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    for code in [0, 1, 2, 42, 127, 255] {
        let out = ws.run(&dist, &["run", "//app:code", "--", &code.to_string()])?;
        assert_eq!(out.status.code(), Some(code), "{}", describe(&out));
    }
    Ok(())
}

/// Piped stdin — binary, every byte value — reaches the program exactly, with
/// its EOF; heph reads none of it.
#[test]
fn entrypoint_stdin_passes_through() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let input: Vec<u8> = (0..=255u8).cycle().take(256 * 64).collect();
    let mut child = ws
        .cmd(&dist, &["run", "//app:cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = {
        let input = input.clone();
        std::thread::spawn(move || {
            stdin.write_all(&input)?;
            drop(stdin);
            std::io::Result::Ok(())
        })
    };
    let out = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_panic| anyhow::anyhow!("the stdin writer panicked"))??;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        out.stdout == input,
        "stdout differs from stdin ({} vs {} bytes)",
        out.stdout.len(),
        input.len()
    );
    Ok(())
}

/// A program killed by a signal kills heph with the same signal — never
/// `exit 128 + n`, which a shell reads differently — and heph dumps no core of
/// its own even when the caller allows one.
#[test]
fn entrypoint_signal_death_is_reraised() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    for (name, number) in [
        ("INT", 2),
        ("QUIT", 3),
        ("ABRT", 6),
        ("KILL", 9),
        ("SEGV", 11),
        ("PIPE", 13),
        ("TERM", 15),
    ] {
        // heph under an unlimited core limit; the program lowers its own, so
        // only a heph core could appear.
        let heph = dist.heph();
        let cmd = ws.cmd(&dist, &[]);
        let envs: Vec<_> = cmd
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
            .collect();
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", "ulimit -c \"$(ulimit -H -c)\"; exec \"$0\" \"$@\""])
            .arg(&heph)
            .args(["run", "//app:die", "--", name])
            .current_dir(ws.root())
            .envs(envs)
            .stdin(Stdio::null())
            .output()?;
        assert_eq!(
            out.status.signal(),
            Some(number),
            "SIG{name}: heph must die of it\n{}",
            describe(&out)
        );
        assert!(!out.status.core_dumped(), "SIG{name}: heph dumped core");
        assert_eq!(out.status.code(), None, "SIG{name}: not an exit code");
    }
    Ok(())
}
