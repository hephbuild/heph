//! `heph run` transfers a selected target's outputs only when a flag reads
//! them (`--cat-out`, `--list-out`, `--copy-out`).
//!
//! On a fully cached CI run, every selected target is a remote hit. Asking for
//! all of their outputs downloaded every one of those blobs into the local
//! cache, and no one read them. Over 91 OCI images, each archive carrying its
//! base's layers, that took minutes on a run with nothing to build.
//!
//! Drives the built `heph` binary, because the choice of what to fetch is the
//! command's: the engine fetches exactly what it is asked for.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Ws {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
    remote: tempfile::TempDir,
}

impl Ws {
    /// A workspace with a writable `file://` remote cache and one target whose
    /// output differs on every execution, so a rebuild is told apart from a hit
    /// by its bytes.
    fn new() -> Self {
        let ws = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            home: tempfile::tempdir().expect("home"),
            remote: tempfile::tempdir().expect("remote"),
        };
        std::fs::write(
            ws.dir.path().join(".hephconfig"),
            format!(
                "plugins:\n  - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
                 - builtin: bash\ncaches:\n  r:\n    uri: file://{}\n    read: true\n    write: true\n",
                ws.remote.path().display()
            ),
        )
        .expect("write config");
        std::fs::write(
            ws.dir.path().join("BUILD"),
            r#"target(name = "t", driver = "bash", run = ["echo $RANDOM$RANDOM$RANDOM > $OUT"], out = "t.txt")"#,
        )
        .expect("write BUILD");
        ws
    }

    fn run(&self, args: &[&str]) -> Output {
        let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_heph")))
            .args(args)
            .current_dir(self.dir.path())
            .env("HOME", self.home.path())
            .env("HEPH_CWD", self.dir.path())
            .env("HEPH_NO_SELF_UPDATE", "1")
            .env("HEPH_DISABLE_TELEMETRY", "1")
            .stdin(Stdio::null())
            .output()
            .expect("run heph");
        assert!(out.status.success(), "heph {args:?}: {}", describe(&out));
        out
    }

    /// Drops the local cache: the next run starts as a fresh CI runner does,
    /// with only the remote warm.
    fn drop_local_cache(&self) {
        std::fs::remove_dir_all(self.dir.path().join(".heph")).expect("remove local cache");
    }

    /// The remote's one output blob: every file of the revision that is not
    /// its manifest.
    fn remote_blob(&self) -> PathBuf {
        let mut blobs = Vec::new();
        collect_files(self.remote.path(), &mut blobs);
        blobs.retain(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("manifest"))
        });
        assert_eq!(blobs.len(), 1, "one output blob on the remote: {blobs:?}");
        blobs.remove(0)
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read remote dir") {
        let path = entry.expect("remote entry").path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn describe(out: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A plain `heph run` on a remote hit does not open the output blob, and
/// `--cat-out` afterwards still gets the bytes the remote holds.
///
/// The remote blob is made unreadable for the plain run. Opening it fails, and
/// a failed pull makes the engine rebuild the target. The rebuild's output
/// differs from the remote's, which the engine reports as a failed run, so a
/// plain run that tried to download the blob fails. A run that did not try to
/// download it succeeds, and `--cat-out` afterwards prints the remote's bytes.
#[test]
fn plain_run_on_a_remote_hit_downloads_no_output() {
    let ws = Ws::new();
    let built = ws.run(&["run", "//:t", "--cat-out"]).stdout;
    assert!(!built.is_empty(), "the target printed its output");

    ws.drop_local_cache();
    let blob = ws.remote_blob();
    set_mode(&blob, 0o000);
    if std::fs::File::open(&blob).is_ok() {
        // Root reads through the mode bits; nothing would fail to open.
        set_mode(&blob, 0o644);
        eprintln!("skipping: an unreadable file is still readable by this user");
        return;
    }
    let plain = ws.run(&["run", "//:t"]);
    set_mode(&blob, 0o644);
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(
        !stderr.contains("blob download"),
        "the plain run tried to download the output: {}",
        describe(&plain)
    );

    let read = ws.run(&["run", "//:t", "--cat-out"]).stdout;
    assert_eq!(
        String::from_utf8_lossy(&read),
        String::from_utf8_lossy(&built),
        "--cat-out reads the remote's bytes, not a rebuild's"
    );
}
