//! Regression test for `wait_with_output_polling` pipe deadlock.
//!
//! Prior implementation joined the wait future and stdout/stderr read futures
//! on the same task via `try_join!`. `wait_polling` enters
//! `block_in_place(|| blocking_recv())`, which parks the worker synchronously
//! inside the task's `poll()` — sibling futures never get polled. A child
//! that writes more than the pipe buffer (64 KiB on macOS) then blocks in
//! `write()` forever, the wait never resolves, and the call hangs.
//!
//! The fix drains pipes on dedicated OS threads. This test would have hung
//! indefinitely against the old code; the timeout asserts that.

#![cfg(unix)]

use std::time::Duration;
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_with_output_drains_large_stdout() {
    // 1 MiB of stdout — far exceeds the 64 KiB pipe buffer.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("yes hello | head -c 1048576")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn");

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        rheph::process_supervisor::wait_with_output_polling(&mut child),
    )
    .await
    .expect("must not hang on large stdout")
    .expect("wait_with_output_polling");

    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 1_048_576);
    assert!(output.stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_with_output_drains_large_stderr() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("yes hello | head -c 1048576 >&2")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn");

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        rheph::process_supervisor::wait_with_output_polling(&mut child),
    )
    .await
    .expect("must not hang on large stderr")
    .expect("wait_with_output_polling");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.len(), 1_048_576);
}
