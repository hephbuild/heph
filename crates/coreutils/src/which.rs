//! `which` — where a command on `PATH` comes from.
//!
//! Not part of coreutils, so uutils has no applet for it. This follows BSD
//! `which`, the one macOS ships: `-a`, `-s`, options only before the first
//! name, a name containing `/` checked as given and printed verbatim, and exit
//! 1 for anything short of every name found (usage errors included).
//!
//! Lookup on `PATH` is the `which` crate's, against the `PATH` this process
//! was given, so inside a target it answers for the target's `PATH` —
//! builtins included — not the host's. One departure from BSD comes with it:
//! a match through a *relative* `PATH` entry is printed absolute.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

const OK: i32 = 0;
const FAILED: i32 = 1;

const HELP: &str = "usage: which [-as] program ...\n\
     \n\
     Prints the path each program resolves to on PATH.\n\
     -a  print every match, not just the first\n\
     -s  print nothing; only set the exit status\n\
     Exits 1 unless every program is found.";

pub(crate) fn main(argv: Vec<OsString>) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let path = std::env::var_os("PATH");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut err = std::io::stderr().lock();
    run(&argv, path.as_deref(), &cwd, &mut out, &mut err)
}

fn run(
    argv: &[OsString],
    path: Option<&OsStr>,
    cwd: &Path,
    out: &mut impl Write,
    err: &mut impl Write,
) -> i32 {
    let mut all = false;
    let mut silent = false;
    let mut args = argv.iter().skip(1).peekable();
    // Options end at the first name or at `--`, as in grep: `which tool -s`
    // looks up a program called `-s`.
    while let Some(&arg) = args.peek() {
        let bytes = arg.as_bytes();
        let Some(flags) = bytes.strip_prefix(b"-").filter(|f| !f.is_empty()) else {
            break;
        };
        args.next();
        if bytes == b"--" {
            break;
        }
        if bytes == b"--help" {
            drop(writeln!(out, "{HELP}"));
            return OK;
        }
        for &flag in flags {
            match flag {
                b'a' => all = true,
                b's' => silent = true,
                _ => {
                    drop(writeln!(
                        err,
                        "which: unknown option {}\n{HELP}",
                        arg.to_string_lossy()
                    ));
                    return FAILED;
                }
            }
        }
    }
    let names: Vec<&OsString> = args.collect();
    if names.is_empty() {
        drop(writeln!(err, "{HELP}"));
        return FAILED;
    }

    let mut code = OK;
    for name in names {
        let found = if name.as_bytes().contains(&b'/') {
            // A path, not a name to search for: checked where it points and
            // printed as given, so `which ./tool` says `./tool`.
            let hit = is_executable_file(&cwd.join(name));
            if hit && !silent {
                print(out, name);
            }
            hit
        } else {
            lookup(name, path, cwd, all, silent, out)
        };
        if !found {
            code = FAILED;
        }
    }
    drop(out.flush());
    code
}

/// Search `path` for `name`, printing the first match or, under `all`, each.
fn lookup(
    name: &OsStr,
    path: Option<&OsStr>,
    cwd: &Path,
    all: bool,
    silent: bool,
    out: &mut impl Write,
) -> bool {
    // An empty name is "not found" rather than whatever the finder makes of it.
    if name.is_empty() {
        return false;
    }
    let Ok(hits) = which::which_in_all(name, path, cwd) else {
        return false;
    };
    let mut found = false;
    for hit in hits {
        found = true;
        if !silent {
            print(out, hit.as_os_str());
        }
        if !all {
            break;
        }
    }
    found
}

fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn print(out: &mut impl Write, p: &OsStr) {
    drop(out.write_all(p.as_bytes()));
    drop(out.write_all(b"\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn exe(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    fn which(args: &[&str], path: &OsStr, cwd: &Path) -> (i32, String) {
        let mut argv = vec![OsString::from("which")];
        argv.extend(args.iter().map(OsString::from));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&argv, Some(path), cwd, &mut out, &mut err);
        (code, String::from_utf8(out).expect("utf8"))
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).expect("join PATH")
    }

    #[test]
    fn first_match_on_path_wins() {
        let t = tempfile::tempdir().expect("tempdir");
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        let want = exe(&a, "tool");
        exe(&b, "tool");
        let (code, out) = which(&["tool"], &path_of(&[&a, &b]), t.path());
        assert_eq!(code, 0);
        assert_eq!(out, format!("{}\n", want.display()));
    }

    #[test]
    fn dash_a_prints_every_match_in_path_order() {
        let t = tempfile::tempdir().expect("tempdir");
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        let first = exe(&a, "tool");
        let second = exe(&b, "tool");
        let (code, out) = which(&["-a", "tool"], &path_of(&[&a, &b]), t.path());
        assert_eq!(code, 0);
        assert_eq!(out, format!("{}\n{}\n", first.display(), second.display()));
    }

    #[test]
    fn a_non_executable_file_is_not_a_match() {
        let t = tempfile::tempdir().expect("tempdir");
        let dir = t.path().join("bin");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("data"), "x").expect("write");
        let (code, out) = which(&["data"], &path_of(&[&dir]), t.path());
        assert_eq!((code, out.as_str()), (1, ""));
    }

    #[test]
    fn any_missing_name_fails_but_the_found_ones_still_print() {
        let t = tempfile::tempdir().expect("tempdir");
        let dir = t.path().join("bin");
        let tool = exe(&dir, "tool");
        let (code, out) = which(&["nope", "tool", ""], &path_of(&[&dir]), t.path());
        assert_eq!(code, 1);
        assert_eq!(out, format!("{}\n", tool.display()));
    }

    #[test]
    fn dash_s_is_silent_and_keeps_the_status() {
        let t = tempfile::tempdir().expect("tempdir");
        let dir = t.path().join("bin");
        exe(&dir, "tool");
        let path = path_of(&[&dir]);
        assert_eq!(which(&["-s", "tool"], &path, t.path()), (0, String::new()));
        assert_eq!(which(&["-s", "nope"], &path, t.path()), (1, String::new()));
    }

    #[test]
    fn usage_errors_exit_1_as_bsd_does() {
        let t = tempfile::tempdir().expect("tempdir");
        let path = path_of(&[t.path()]);
        assert_eq!(which(&[], &path, t.path()).0, 1);
        assert_eq!(which(&["-x", "tool"], &path, t.path()).0, 1);
        assert_eq!(which(&["--all", "tool"], &path, t.path()).0, 1);
    }

    #[test]
    fn options_end_at_the_first_name() {
        let t = tempfile::tempdir().expect("tempdir");
        let dir = t.path().join("bin");
        let tool = exe(&dir, "tool");
        // `-s` after a name is a program to look up, not "be silent".
        let (code, out) = which(&["tool", "-s"], &path_of(&[&dir]), t.path());
        assert_eq!(code, 1);
        assert_eq!(out, format!("{}\n", tool.display()));
    }

    #[test]
    fn a_name_with_a_slash_is_checked_and_printed_as_given() {
        let t = tempfile::tempdir().expect("tempdir");
        exe(&t.path().join("rel"), "tool");
        std::fs::write(t.path().join("rel/data"), "x").expect("write");
        let empty = path_of(&[&t.path().join("nowhere")]);
        assert_eq!(
            which(&["./rel/tool", "rel/tool"], &empty, t.path()),
            (0, "./rel/tool\nrel/tool\n".to_string())
        );
        assert_eq!(which(&["rel/data"], &empty, t.path()), (1, String::new()));
        assert_eq!(which(&["rel"], &empty, t.path()), (1, String::new()));
    }

    #[test]
    fn after_double_dash_a_dash_name_is_a_name() {
        let t = tempfile::tempdir().expect("tempdir");
        let dir = t.path().join("bin");
        let tool = exe(&dir, "-a");
        let (code, out) = which(&["--", "-a"], &path_of(&[&dir]), t.path());
        assert_eq!(code, 0);
        assert_eq!(out, format!("{}\n", tool.display()));
    }
}
