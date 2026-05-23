//! Regression test for the pipe-drain deadlock that the previous polling
//! implementation suffered from: joining wait and stdout/stderr reads on
//! the same task blocked the IO drains when the wait parked the worker
//! synchronously. The replacement `proc_exec::output` drains on dedicated
//! `std::thread`s, which decouples the wait from the IO completely.

#![cfg(unix)]

use rheph::hasync::StdCancellationToken;
use rheph::proc_exec;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn output_drains_large_stdout() {
    // 1 MiB of stdout — far exceeds the 64 KiB pipe buffer.
    let spec = proc_exec::Spec {
        program: PathBuf::from("sh"),
        args: vec![
            OsString::from("-c"),
            OsString::from("yes hello | head -c 1048576"),
        ],
        env: Vec::new(),
        cwd: std::env::current_dir().expect("cwd"),
        stdin: proc_exec::StdioSpec::Null,
        stdout: proc_exec::StdioSpec::Piped,
        stderr: proc_exec::StdioSpec::Piped,
        setsid: false,
        ctty: false,
    };
    let ctoken = StdCancellationToken::new();
    let output = tokio::time::timeout(Duration::from_secs(10), proc_exec::output(spec, &ctoken))
        .await
        .expect("must not hang on large stdout")
        .expect("proc_exec::output");

    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 1_048_576);
    assert!(output.stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn output_drains_large_stderr() {
    let spec = proc_exec::Spec {
        program: PathBuf::from("sh"),
        args: vec![
            OsString::from("-c"),
            OsString::from("yes hello | head -c 1048576 >&2"),
        ],
        env: Vec::new(),
        cwd: std::env::current_dir().expect("cwd"),
        stdin: proc_exec::StdioSpec::Null,
        stdout: proc_exec::StdioSpec::Piped,
        stderr: proc_exec::StdioSpec::Piped,
        setsid: false,
        ctty: false,
    };
    let ctoken = StdCancellationToken::new();
    let output = tokio::time::timeout(Duration::from_secs(10), proc_exec::output(spec, &ctoken))
        .await
        .expect("must not hang on large stderr")
        .expect("proc_exec::output");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.len(), 1_048_576);
}
