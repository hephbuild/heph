//! `which` — where a command on `PATH` comes from.
//!
//! Not part of coreutils, so uutils has no applet for it. GNU `which` and BSD
//! `which` agree on the useful core (`-a`, `-s`, exit 1 when anything is
//! missing), and that core is what ships. Resolution is the `which` crate's,
//! against the `PATH` this process was given, so inside a target it answers
//! for the target's `PATH` — builtins included — not the host's.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;

const OK: i32 = 0;
const NOT_FOUND: i32 = 1;
const USAGE: i32 = 2;

const HELP: &str = "usage: which [-as] program ...\n\
     \n\
     Prints the path each program resolves to on PATH.\n\
     -a  print every match, not just the first\n\
     -s  print nothing; only set the exit status\n\
     Exits 1 if any program is not found.";

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
    let mut names = Vec::new();
    let mut flags_done = false;
    for arg in argv.iter().skip(1) {
        let bytes = arg.as_encoded_bytes();
        let flags = match bytes.strip_prefix(b"-") {
            Some(rest) if !flags_done && !rest.is_empty() => rest,
            _ => {
                names.push(arg);
                continue;
            }
        };
        if bytes == b"--" {
            flags_done = true;
            continue;
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
                        "which: unknown option -{}\n{HELP}",
                        char::from(flag)
                    ));
                    return USAGE;
                }
            }
        }
    }
    if names.is_empty() {
        drop(writeln!(err, "{HELP}"));
        return USAGE;
    }

    let mut code = OK;
    for name in names {
        let mut found = false;
        // An empty name is "not found" rather than whatever the finder makes
        // of it; GNU and BSD both say so.
        if !name.is_empty()
            && let Ok(hits) = which::which_in_all(name, path, cwd)
        {
            for hit in hits {
                found = true;
                if !silent {
                    drop(out.write_all(hit.as_os_str().as_encoded_bytes()));
                    drop(out.write_all(b"\n"));
                }
                if !all {
                    break;
                }
            }
        }
        if !found {
            code = NOT_FOUND;
        }
    }
    drop(out.flush());
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
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
    fn usage_errors_exit_2() {
        let t = tempfile::tempdir().expect("tempdir");
        let path = path_of(&[t.path()]);
        assert_eq!(which(&[], &path, t.path()).0, 2);
        assert_eq!(which(&["-x", "tool"], &path, t.path()).0, 2);
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
