//! The builtin applets whose contract is what they print.
//!
//! The coreutils crate's own tests run applets in-process and can only see
//! their effect on the filesystem: an applet owns the process's stdout. These
//! spawn the built `heph`, through a shim symlink named after the applet —
//! the `argv[0]` dispatch a target's `PATH` actually reaches.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

/// A shim directory holding a symlink to `heph` for each of `names`.
fn shims(names: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("shim dir");
    for name in names {
        std::os::unix::fs::symlink(heph_bin(), dir.path().join(name)).expect("symlink");
    }
    dir
}

/// Run applet `name` from `shims`, with only `shims` on `PATH`, in `cwd`.
fn run(shims: &Path, cwd: &Path, name: &str, args: &[&str]) -> Output {
    Command::new(shims.join(name))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", shims)
        .env("FROM_THE_TARGET", "yes")
        .stdin(Stdio::null())
        .output()
        .expect("spawn the applet's shim")
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "exit {}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).expect("utf8 stdout")
}

#[test]
fn ls_lists_a_directory_one_name_per_line_sorted() {
    let bin = shims(&["ls"]);
    let ws = tempfile::tempdir().expect("ws");
    for name in ["b", "a", "C", ".hidden"] {
        std::fs::write(ws.path().join(name), "").expect("write");
    }
    std::fs::create_dir(ws.path().join("dir")).expect("mkdir");
    // Not a tty, so one per line and no colour; C-locale order puts `C` first.
    assert_eq!(
        stdout(&run(bin.path(), ws.path(), "ls", &[])),
        "C\na\nb\ndir\n"
    );
    assert_eq!(
        stdout(&run(bin.path(), ws.path(), "ls", &["-A"])),
        ".hidden\nC\na\nb\ndir\n"
    );
    let long = stdout(&run(bin.path(), ws.path(), "ls", &["-l", "dir"]));
    assert!(
        long.starts_with("total 0"),
        "ls -l of an empty dir: {long:?}"
    );
}

#[test]
fn pwd_prints_the_working_directory() {
    let bin = shims(&["pwd"]);
    let ws = tempfile::tempdir().expect("ws");
    let real = ws.path().canonicalize().expect("canonicalize");
    let got = stdout(&run(bin.path(), ws.path(), "pwd", &["-P"]));
    assert_eq!(got, format!("{}\n", real.display()));
}

#[test]
fn test_answers_through_its_exit_code() {
    let bin = shims(&["test"]);
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("f"), "x").expect("write");
    let code = |args: &[&str]| run(bin.path(), ws.path(), "test", args).status.code();
    assert_eq!(code(&["-f", "f"]), Some(0));
    assert_eq!(code(&["-d", "f"]), Some(1));
    assert_eq!(code(&["-s", "f", "-a", "a", "=", "a"]), Some(0));
    assert_eq!(code(&["1", "-lt", "2"]), Some(0));
}

#[test]
fn which_answers_for_the_path_it_was_given() {
    let bin = shims(&["which", "ls"]);
    let ws = tempfile::tempdir().expect("ws");
    let got = stdout(&run(bin.path(), ws.path(), "which", &["ls"]));
    assert_eq!(got, format!("{}\n", bin.path().join("ls").display()));
    // The host's directories are not on this PATH, so the host's tools are
    // not found — `which` reports the target's view, not the machine's.
    let out = run(bin.path(), ws.path(), "which", &["ls", "git"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn printenv_reads_the_environment_it_was_given() {
    let bin = shims(&["printenv"]);
    let ws = tempfile::tempdir().expect("ws");
    let got = stdout(&run(
        bin.path(),
        ws.path(),
        "printenv",
        &["FROM_THE_TARGET"],
    ));
    assert_eq!(got, "yes\n");
    let missing = run(bin.path(), ws.path(), "printenv", &["NOT_SET"]);
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn checksums_and_od_print_known_vectors() {
    let bin = shims(&["cksum", "b2sum", "od"]);
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("empty"), "").expect("write");
    std::fs::write(ws.path().join("ab"), "ab").expect("write");
    assert_eq!(
        stdout(&run(bin.path(), ws.path(), "cksum", &["empty"])),
        "4294967295 0 empty\n"
    );
    assert_eq!(
        stdout(&run(bin.path(), ws.path(), "b2sum", &["empty"])),
        "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419\
         d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce  empty\n"
    );
    assert_eq!(
        stdout(&run(bin.path(), ws.path(), "od", &["-An", "-tx1", "ab"])),
        " 61 62\n"
    );
}

#[test]
fn du_counts_apparent_bytes() {
    let bin = shims(&["du"]);
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("f"), vec![b'x'; 1000]).expect("write");
    assert_eq!(
        stdout(&run(bin.path(), ws.path(), "du", &["-b", "f"])),
        "1000\tf\n"
    );
}

#[test]
fn yes_repeats_until_its_reader_stops() {
    let bin = shims(&["yes", "head"]);
    let ws = tempfile::tempdir().expect("ws");
    // `yes | head -n 2`, without a shell: yes must stop on the closed pipe
    // rather than spin, or the `wait` below hangs.
    let mut yes = Command::new(bin.path().join("yes"))
        .arg("y")
        .env_clear()
        .stdout(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn yes");
    let pipe = yes.stdout.take().expect("yes stdout");
    let head = Command::new(bin.path().join("head"))
        .args(["-n", "2"])
        .current_dir(ws.path())
        .env_clear()
        .stdin(pipe)
        .output()
        .expect("run head");
    assert_eq!(stdout(&head), "y\ny\n");
    yes.wait().expect("wait yes");
}

#[test]
fn identity_applets_agree_with_the_process() {
    let bin = shims(&["id", "whoami", "groups"]);
    let ws = tempfile::tempdir().expect("ws");
    // A file this test creates is owned by the effective uid, which is what
    // `id -u` reports — no libc call needed to know the right answer.
    let probe = ws.path().join("probe");
    std::fs::write(&probe, "").expect("write");
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&probe).expect("stat"));
    assert_eq!(
        stdout(&run(bin.path(), ws.path(), "id", &["-u"])),
        format!("{uid}\n")
    );
    let name = stdout(&run(bin.path(), ws.path(), "id", &["-un"]));
    assert_eq!(stdout(&run(bin.path(), ws.path(), "whoami", &[])), name);
    let groups = stdout(&run(bin.path(), ws.path(), "groups", &[]));
    assert!(!groups.trim().is_empty(), "groups printed nothing");
}
