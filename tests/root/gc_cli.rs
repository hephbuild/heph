//! `heph tool gc --failure-max-age-days`: the flag parses, and the summary
//! line counts the failure logs it removed and the bytes they held.
//!
//! Drives the built `heph` binary, because both halves are the CLI's: the
//! flag's parsing and the CI view's summary line.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

#[test]
fn gc_failure_max_age_flag() {
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

    let out = Command::new(heph_bin())
        .args(["tool", "gc", "--failure-max-age-days", "0"])
        .current_dir(ws.path())
        .env("HOME", home.path())
        .env("HEPH_CWD", ws.path())
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph tool gc");
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
