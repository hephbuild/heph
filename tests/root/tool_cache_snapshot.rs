//! `heph tool cache snapshot`: its `--json` document and its exit codes
//! (D15).
//!
//! Drives the built `heph` binary, because both are the CLI's: the exit code
//! a CI script reads and the document on stdout. The lock protocol is
//! covered in-process (`crates/e2e`, `cache_snapshot::`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
            ws.dir.path().join(".hephconfig"),
            "plugins:\n  - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
             - builtin: bash\n",
        )
        .expect("write config");
        std::fs::write(
            ws.dir.path().join("BUILD"),
            "target(name = \"a\", driver = \"bash\", run = [\"echo a > $OUT\"], out = \"a.txt\")\n\
             target(name = \"b\", driver = \"bash\", run = [\"echo bb > $OUT\"], out = \"b.txt\")\n",
        )
        .expect("write BUILD");
        ws
    }

    /// `heph <args>` with the snapshot dir `snapshot` (`None`: not
    /// configured).
    fn heph(&self, snapshot: Option<&Path>, args: &[&str]) -> Output {
        let mut cmd = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_heph")));
        cmd.args(args)
            .current_dir(self.dir.path())
            .env("HOME", self.home.path())
            .env("HEPH_CWD", self.dir.path())
            .env("HEPH_NO_SELF_UPDATE", "1")
            .env("HEPH_DISABLE_TELEMETRY", "1")
            .env_remove("CI")
            .stdin(Stdio::null());
        match snapshot {
            Some(dir) => cmd.env("HEPH_CACHE_SNAPSHOT", dir),
            None => cmd.env_remove("HEPH_CACHE_SNAPSHOT"),
        };
        cmd.output().expect("run heph")
    }

    fn snapshot(&self, dir: Option<&Path>) -> (i32, serde_json::Value, Output) {
        let out = self.heph(dir, &["tool", "cache", "snapshot", "--json"]);
        let code = out.status.code().expect("an exit code");
        let doc = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("stdout is one JSON document ({e}): {}", describe(&out)));
        (code, doc, out)
    }

    /// One `heph run` per address, each recording its own journal.
    fn build(&self, dir: &Path, addrs: &[&str]) {
        for addr in addrs {
            let out = self.heph(Some(dir), &["run", addr]);
            assert!(out.status.success(), "heph run {addr}: {}", describe(&out));
        }
    }

    fn journals(&self) -> Vec<PathBuf> {
        let dir = self.dir.path().join(".heph/cache/working-set");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
            .collect();
        out.sort();
        out
    }
}

fn describe(out: &Output) -> String {
    format!(
        "{}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn u64_at(doc: &serde_json::Value, pointer: &str) -> u64 {
    doc.pointer(pointer)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("{pointer} is an integer: {doc}"))
}

#[test]
fn json_report_and_exit_codes() {
    let ws = Ws::new();
    let snap = ws.dir.path().join("snap");

    // 1: not configured.
    let (code, doc, out) = ws.snapshot(None);
    assert_eq!(code, 1, "{}", describe(&out));
    assert_eq!(doc["schema"], 1, "{doc}");
    assert_eq!(doc["reason"], "not_configured", "{doc}");

    // 0, written: the journals' revisions, in a new generation.
    ws.build(&snap, &["//:a", "//:b"]);
    assert_eq!(ws.journals().len(), 2);
    let (code, doc, out) = ws.snapshot(Some(&snap));
    assert_eq!(code, 0, "{}", describe(&out));
    assert_eq!(doc["schema"], 1, "{doc}");
    assert!(doc["schema"].is_u64(), "{doc}");
    assert_eq!(doc["dir"], snap.to_str().expect("utf-8 dir"), "{doc}");
    assert_eq!(doc["outcome"]["kind"], "written", "{doc}");
    assert_eq!(doc["complete"], true, "{doc}");
    assert_eq!(u64_at(&doc, "/new/count"), 2, "{doc}");
    assert!(u64_at(&doc, "/new/bytes") > 0, "{doc}");
    for tally in ["kept", "carried_over", "dropped"] {
        assert_eq!(u64_at(&doc, &format!("/{tally}/count")), 0, "{doc}");
        u64_at(&doc, &format!("/{tally}/bytes"));
    }
    assert_eq!(doc["left_out"], serde_json::json!({}), "{doc}");
    assert_eq!(doc["incomplete"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["journals"].as_array().map(Vec::len), Some(2), "{doc}");
    // The top revisions by bytes, largest first, ties by addr.
    let top = doc["top"].as_array().expect("top is an array");
    assert_eq!(top.len(), 2, "{doc}");
    let sizes: Vec<u64> = (0..top.len())
        .map(|i| u64_at(&doc, &format!("/top/{i}/bytes")))
        .collect();
    assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "{doc}");
    assert!(top.iter().all(|r| r["hashin"].is_string()), "{doc}");
    assert_eq!(doc["dropped_addrs"], serde_json::json!([]), "{doc}");
    assert!(ws.journals().is_empty(), "the journal was consumed");
    // Stable: the document's top-level keys come in a fixed order, and there
    // are no others (no timestamps).
    let stdout = String::from_utf8_lossy(&out.stdout);
    let order = [
        "schema",
        "dir",
        "complete",
        "outcome",
        "kept",
        "new",
        "carried_over",
        "dropped",
        "left_out",
        "journals",
        "incomplete",
        "top",
        "dropped_addrs",
        "scratch",
        "cleanup_errors",
    ];
    let at: Vec<usize> = order
        .iter()
        .map(|k| {
            stdout
                .find(&format!("\"{k}\":"))
                .unwrap_or_else(|| panic!("{k} in {stdout}"))
        })
        .collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{stdout}");
    let mut keys: Vec<&str> = doc
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut want = order.to_vec();
    want.sort_unstable();
    assert_eq!(keys, want, "{doc}");

    // 0, written again with one target: the other is dropped and named.
    ws.build(&snap, &["//:a"]);
    let (code, doc, out) = ws.snapshot(Some(&snap));
    assert_eq!(code, 0, "{}", describe(&out));
    assert_eq!(u64_at(&doc, "/kept/count"), 1, "{doc}");
    assert_eq!(u64_at(&doc, "/dropped/count"), 1, "{doc}");
    assert_eq!(doc["dropped_addrs"], serde_json::json!(["//:b"]), "{doc}");

    // 0, unchanged: no journal, the dir as it was.
    let (code, doc, out) = ws.snapshot(Some(&snap));
    assert_eq!(code, 0, "{}", describe(&out));
    assert_eq!(doc["outcome"]["kind"], "unchanged", "{doc}");

    // 0, incomplete: a journal without its trailer is flagged, not refused.
    ws.build(&snap, &["//:a"]);
    let journal = ws.journals().pop().expect("a journal");
    let text = std::fs::read_to_string(&journal).expect("read journal");
    let cut: String = text
        .lines()
        .filter(|l| !l.contains(r#""t":"Trailer""#))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&journal, cut).expect("strip the trailer");
    let (code, doc, out) = ws.snapshot(Some(&snap));
    assert_eq!(code, 0, "{}", describe(&out));
    assert_eq!(doc["complete"], false, "{doc}");
    assert_eq!(doc["incomplete"].as_array().map(Vec::len), Some(1), "{doc}");
    // The human report flags it too.
    ws.build(&snap, &["//:a"]);
    let journal = ws.journals().pop().expect("a journal");
    let text = std::fs::read_to_string(&journal).expect("read journal");
    std::fs::write(&journal, text.replace("{\"t\":\"Trailer\"}\n", "")).expect("strip");
    let out = ws.heph(Some(&snap), &["tool", "cache", "snapshot"]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("snapshot: wrote gen-"), "{stdout}");
    assert!(stdout.contains("incomplete: "), "{stdout}");

    // 4: a journal from a newer heph; the message names it and says deleting
    // it is safe. Nothing is consumed.
    let newer = ws.dir.path().join(".heph/cache/working-set/1-newer.log");
    std::fs::write(&newer, "{\"magic\":\"heph-working-set\",\"version\":999}\n")
        .expect("write newer journal");
    let (code, doc, out) = ws.snapshot(Some(&snap));
    assert_eq!(code, 4, "{}", describe(&out));
    assert_eq!(doc["reason"], "newer_journal", "{doc}");
    let message = doc["error"].as_str().expect("error is a string");
    assert!(message.contains("1-newer.log"), "{message}");
    assert!(message.contains("Deleting it is safe"), "{message}");
    assert!(newer.exists(), "a refused journal is left in place");
    std::fs::remove_file(&newer).expect("delete newer journal");

    // 2: a dir that cannot be written (a file stands where it should be).
    let blocked = ws.dir.path().join("blocked");
    std::fs::write(&blocked, "not a dir").expect("write blocker");
    ws.build(&blocked, &["//:b"]);
    let (code, doc, out) = ws.snapshot(Some(&blocked));
    assert_eq!(code, 2, "{}", describe(&out));
    assert_eq!(doc["reason"], "error", "{doc}");
    assert_eq!(ws.journals().len(), 1, "a failed export consumes nothing");

    // 2: a usage error.
    let out = ws.heph(Some(&snap), &["tool", "cache", "snapshot", "--nope"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
}
