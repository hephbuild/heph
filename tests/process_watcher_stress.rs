//! Stress test for `proc_exec` — spawns many concurrent short-lived
//! children and verifies every one resolves with the real exit status.
//! This is the load profile that triggered the macOS
//! `tokio::process::Child::wait` starvation documented in
//! `RCA_MACOS_WAKER.md`.

#![cfg(unix)]

use heph::hasync::StdCancellationToken;
use heph::proc_exec;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

const N: usize = 500;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_short_lived_children_all_resolve_with_status() {
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let exit_code = (i % 3) as i32;
        handles.push(tokio::spawn(async move {
            let spec = proc_exec::Spec {
                program: PathBuf::from("sh"),
                args: vec![
                    OsString::from("-c"),
                    OsString::from(format!("exit {exit_code}")),
                ],
                env: Vec::new(),
                cwd: std::env::current_dir().expect("cwd"),
                stdin: proc_exec::StdioSpec::Null,
                stdout: proc_exec::StdioSpec::Null,
                stderr: proc_exec::StdioSpec::Null,
                setsid: false,
                ctty: false,
            };
            let ctoken = StdCancellationToken::new();
            let output = proc_exec::output(spec, &ctoken)
                .await
                .expect("proc_exec::output");
            (output.status, exit_code)
        }));
    }

    let results = tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(handles))
        .await
        .expect("all children must resolve within 30s");

    for (idx, r) in results.into_iter().enumerate() {
        let (status, expected_code) = r.expect("task panicked");
        assert!(
            status.code() == Some(expected_code),
            "child #{idx}: expected exit {expected_code}, got {status:?} \
             (None means ECHILD path synthesized success — watcher lost the status)"
        );
    }
}
