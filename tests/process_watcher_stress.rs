//! Stress test for `process_watcher` — spawns many concurrent
//! short-lived children and verifies every one resolves with the real
//! exit status. This is the load profile that triggered the macOS
//! `tokio::process::Child::wait` starvation documented in
//! `RCA_MACOS_WAKER.md`.

#![cfg(unix)]

use std::time::Duration;
use tokio::process::Command;

const N: usize = 500;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_short_lived_children_all_resolve_with_status() {
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let exit_code = (i % 3) as i32;
        handles.push(tokio::spawn(async move {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(format!("exit {exit_code}"))
                .kill_on_drop(true);
            let mut child = cmd.spawn().expect("spawn sh");
            let status = rheph::process_supervisor::wait_polling(&mut child)
                .await
                .expect("wait_polling");
            (status, exit_code)
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
