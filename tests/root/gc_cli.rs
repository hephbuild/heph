//! `heph tool gc --failure-max-age-days`: the flag parses, and the summary
//! line counts the failure logs it removed and the bytes they held.
//!
//! Drives the built `heph` binary, because both halves are the CLI's: the
//! flag's parsing and the CI view's summary line.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

/// A workspace (and a `HOME`) holding one recent failure slot for a target
/// that exists nowhere, the slot's path and its whole footprint in bytes.
fn workspace_with_a_failure_slot() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, u64) {
    let ws = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        ws.path().join(".hephconfig2"),
        "plugins:\n  - builtin: buildfile\n  - builtin: exec\n",
    )
    .expect("write config");

    // A slot as a failed run leaves it, recent, for a target that exists
    // nowhere: removed under any age, and under `0` without resolving.
    let addr = heph::htaddr::parse_addr("//ghost:t").expect("addr");
    let slot = ws
        .path()
        .join(".heph/failures")
        .join(heph::engine::failure_slot::addr_key(&addr));
    std::fs::create_dir_all(&slot).expect("mkdir slot");
    std::fs::write(slot.join("log.txt"), vec![b'x'; 2048]).expect("log");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_millis();
    let manifest =
        format!(r#"{{"version":1,"addr":"//ghost:t","hashin":"h","failed_at":{now_ms}}}"#);
    std::fs::write(slot.join("manifest.json"), &manifest).expect("manifest");
    // The slot's whole footprint: its log and its manifest.
    let bytes = 2048 + manifest.len() as u64;
    (ws, home, slot, bytes)
}

/// `heph tool gc --failure-max-age-days 0 <extra>` in `ws`.
fn gc_all_failures(ws: &Path, home: &Path, extra: &[&str]) -> Output {
    Command::new(heph_bin())
        .args(["tool", "gc", "--failure-max-age-days", "0"])
        .args(extra)
        .current_dir(ws)
        .env("HOME", home)
        .env("HEPH_CWD", ws)
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph tool gc")
}

#[test]
fn gc_failure_max_age_flag() {
    let (ws, home, slot, bytes) = workspace_with_a_failure_slot();
    let out = gc_all_failures(ws.path(), home.path(), &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "heph tool gc failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    assert!(!slot.exists(), "the slot was removed");
    let all = format!("{stdout}{stderr}");
    let want = format!(
        "0 targets · 0 B freed · 1 failure logs, {} freed",
        heph::tui::human_bytes(bytes)
    );
    assert!(
        all.contains(&want),
        "the summary counts the log and its bytes:\n{all}"
    );

    let bad = Command::new(heph_bin())
        .args(["tool", "gc", "--failure-max-age-days", "soon"])
        .current_dir(ws.path())
        .env("HOME", home.path())
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph tool gc");
    assert!(!bad.status.success(), "a non-number is refused");
}

/// Under `--json`, stdout is the sweep's record and nothing else; the
/// summary line stays on stderr.
#[test]
fn gc_json_is_the_record_alone() {
    let (ws, home, slot, bytes) = workspace_with_a_failure_slot();
    let out = gc_all_failures(ws.path(), home.path(), &["--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "heph tool gc --json failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    assert!(!slot.exists(), "the slot was removed");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}):\n{stdout}"));
    assert_eq!(doc["failure_logs_removed"], 1, "{doc}");
    assert_eq!(doc["failure_log_bytes_removed"], bytes, "{doc}");
    assert_eq!(doc["revisions_removed"], 0, "{doc}");
}
