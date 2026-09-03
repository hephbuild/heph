//! POSIX utilities compiled into the `heph` binary.
//!
//! A build recipe that runs `cp`, `install` or `sha256sum` must behave the same
//! on Linux and macOS. The host userlands do not agree — GNU coreutils on one,
//! a BSD userland on the other — and the divergences are not exotic: `install
//! -D` does not exist on macOS at all, `sed -i` takes its suffix differently,
//! `wc` pads its output, and `sort` collates by locale, which silently changes
//! build *outputs* rather than just exit codes.
//!
//! So heph ships its own. The applets are [uutils/coreutils] crates, MIT
//! licensed and tested against the GNU test suite, reached through a multicall
//! dispatch: the `heph` binary is re-executed with the applet's name, either as
//! `heph __coreutils <applet> …` or through a symlink named after the applet
//! (`argv[0]` dispatch, busybox style), which is what [`shim_dir`] materializes.
//!
//! [uutils/coreutils]: https://github.com/uutils/coreutils

use anyhow::Context as _;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Identity of the builtin toolbox, as it reaches a target's cache key.
///
/// Every `exec`/`bash` target folds this into its def hash, because the applets
/// are on its `PATH` without being declared and nothing can tell which of them a
/// shell command will actually invoke without parsing it. The consequence is
/// blunt and deliberate: **bumping this invalidates every exec target in every
/// workspace**, so it is a release-gated decision, not a routine one.
///
/// Bump it on any observable behaviour change — an applet added or removed, an
/// upstream upgrade that changes output, a fix to one of the hand-written parts.
pub const COREUTILS_VERSION: u32 = 1;

/// Where the applets come from, for `heph tool coreutils list`.
pub const UPSTREAM: &str = "uutils/coreutils 0.10";

/// One utility, and the entry point that runs it.
#[derive(Debug)]
pub struct Applet {
    /// The name it is invoked by — the `PATH` entry, and `argv[0]`.
    pub name: &'static str,
    /// `argv` including the program name at index 0, as every `uumain` expects.
    run: fn(Vec<OsString>) -> i32,
}

/// Build the applet table.
///
/// Each `uumain` takes an `impl uucore::Args`, so it is generic and cannot be
/// named with a turbofish; the wrapper `fn` pins the iterator type and gives us
/// something that coerces to a plain function pointer.
macro_rules! applets {
    ($($name:literal => $krate:ident,)*) => {
        /// Every applet, ordered by name so `list` and the docs agree.
        pub const APPLETS: &[Applet] = &[
            $(Applet {
                name: $name,
                run: {
                    fn run(args: Vec<OsString>) -> i32 { $krate::uumain(args.into_iter()) }
                    run
                },
            },)*
        ];
    };
}

applets! {
    "base64" => uu_base64,
    "basename" => uu_basename,
    "cat" => uu_cat,
    "chmod" => uu_chmod,
    "comm" => uu_comm,
    "cp" => uu_cp,
    "cut" => uu_cut,
    "date" => uu_date,
    "dirname" => uu_dirname,
    "echo" => uu_echo,
    "env" => uu_env,
    "false" => uu_false,
    "head" => uu_head,
    "install" => uu_install,
    "ln" => uu_ln,
    "md5sum" => uu_md5sum,
    "mkdir" => uu_mkdir,
    "mktemp" => uu_mktemp,
    "mv" => uu_mv,
    "nproc" => uu_nproc,
    "printf" => uu_printf,
    "readlink" => uu_readlink,
    "realpath" => uu_realpath,
    "rm" => uu_rm,
    "rmdir" => uu_rmdir,
    "seq" => uu_seq,
    "sha1sum" => uu_sha1sum,
    "sha256sum" => uu_sha256sum,
    "sha512sum" => uu_sha512sum,
    "sleep" => uu_sleep,
    "sort" => uu_sort,
    "stat" => uu_stat,
    "tail" => uu_tail,
    "tee" => uu_tee,
    "timeout" => uu_timeout,
    "touch" => uu_touch,
    "tr" => uu_tr,
    "true" => uu_true,
    "uniq" => uu_uniq,
    "wc" => uu_wc,
}

/// The applet called `name`, if heph ships one.
pub fn find(name: &str) -> Option<&'static Applet> {
    APPLETS.iter().find(|a| a.name == name)
}

/// True if `name` is one of ours — the `argv[0]` test on the multicall path.
pub fn is_applet(name: &str) -> bool {
    find(name).is_some()
}

/// Run `name` with `argv` (program name at index 0), or `None` if we don't ship it.
///
/// The caller is always a dedicated process, so an applet owning stdout/stderr
/// or calling `exit` is contained — which is why this must never be invoked
/// from inside the engine. That is not only a tidiness argument: `uucore`
/// keeps the exit code in a process-global, so two applets run in one process
/// can see each other's failures. One process per invocation makes that
/// unobservable; the tests below have to work around it explicitly.
pub fn dispatch(name: &str, argv: Vec<OsString>) -> Option<i32> {
    find(name).map(|a| (a.run)(argv))
}

/// Directory name for the shim set belonging to `exe`.
///
/// Keyed on the version *and* the binary's path: the shims are symlinks to a
/// specific `heph`, so two installations sharing a home directory must not
/// share a shim directory. A self-update that rewrites the binary in place
/// keeps the same path, and the symlinks stay correct.
fn shim_dir_name(exe: &Path) -> String {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(exe.as_os_str().as_encoded_bytes());
    format!("v{COREUTILS_VERSION}-{:016x}", h.digest())
}

/// Materialize the shim directory for `exe` under `home`, and return its path.
///
/// One symlink per applet, pointing at the `heph` binary, so running `cp` execs
/// heph with `argv[0] == "cp"` — one process, no wrapper script. Written once
/// per (version, binary) and reused forever after: the common call is a single
/// `stat`, and nothing is written per target or per sandbox.
///
/// Concurrency-safe by construction. The content is a pure function of the
/// directory's own name, so a populated directory is always complete and
/// correct; a builder stages into a unique sibling and renames, and loses a
/// race harmlessly because the winner's content is identical.
pub fn shim_dir(home: &Path, exe: &Path) -> anyhow::Result<PathBuf> {
    let root = home.join("coreutils");
    let dir = root.join(shim_dir_name(exe)).join("bin");
    if dir.is_dir() {
        return Ok(dir);
    }

    let staging = root.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        shim_dir_name(exe)
    ));
    // A leftover from a process that died mid-build: its content is keyed the
    // same way, but only a *renamed* directory is known-complete.
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clear stale coreutils staging dir {staging:?}"))?;
    }
    let staging_bin = staging.join("bin");
    std::fs::create_dir_all(&staging_bin)
        .with_context(|| format!("create coreutils staging dir {staging_bin:?}"))?;
    for applet in APPLETS {
        let link = staging_bin.join(applet.name);
        symlink(exe, &link)
            .with_context(|| format!("link coreutils shim {:?} -> {exe:?}", applet.name))?;
    }

    let final_dir = root.join(shim_dir_name(exe));
    match std::fs::rename(&staging, &final_dir) {
        Ok(()) => {}
        Err(e) => {
            // Another heph won the race, or put a directory there between our
            // `is_dir` check and now. Either way its content is ours.
            // Best-effort: the staging dir is ours and now useless, but
            // failing to remove it must not fail the build — the next run
            // clears it by name before rebuilding.
            drop(std::fs::remove_dir_all(&staging));
            if !dir.is_dir() {
                return Err(e).with_context(|| {
                    format!("publish coreutils shim dir {staging:?} -> {final_dir:?}")
                });
            }
        }
    }
    Ok(dir)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Serializes the golden cases; see the comment in `golden`.
    static GOLDEN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    /// A normalized listing of `dir`: every path below it, with its kind and,
    /// for a regular file, its size and whether it is executable.
    ///
    /// This is the golden form. The promise is that Linux and macOS agree, so
    /// what a case records has to be the *effect* on the tree, not a rendering
    /// of it — no mode bits beyond the executable one (umask differs), no
    /// timestamps, no device or inode numbers.
    fn tree(dir: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let md = std::fs::symlink_metadata(&path).unwrap();
                if md.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    out.insert(format!("{rel} -> {}", target.to_string_lossy()));
                } else if md.is_dir() {
                    out.insert(format!("{rel}/"));
                    walk(root, &path, out);
                } else {
                    let x = {
                        use std::os::unix::fs::PermissionsExt as _;
                        md.permissions().mode() & 0o111 != 0
                    };
                    out.insert(format!(
                        "{rel} ({} bytes{})",
                        md.len(),
                        if x { ", executable" } else { "" }
                    ));
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(dir, dir, &mut out);
        out.into_iter().collect()
    }

    /// Run `case` in a fresh tree and assert the exit code and resulting tree.
    ///
    /// Every one of these is asserted identically on `linux/amd64`,
    /// `linux/arm64` and `darwin/arm64` — CI runs the suite natively on all
    /// three — so cross-OS equality is checked by construction rather than by a
    /// comparison the test would have to make itself.
    fn golden(setup: impl Fn(&Path), args: &[&str], code: i32, expect: &[&str]) {
        golden_inner(setup, args, code, expect, false);
    }

    /// A case that must run *from inside* the tree, because the flag under test
    /// is defined on relative operands (`cp --parents`). The process cwd is
    /// global, so this is only safe because every golden case holds `GOLDEN`;
    /// nothing else in this module reads the cwd.
    fn golden_cwd(setup: impl Fn(&Path), args: &[&str], code: i32, expect: &[&str]) {
        golden_inner(setup, args, code, expect, true);
    }

    fn golden_inner(setup: impl Fn(&Path), args: &[&str], code: i32, expect: &[&str], chdir: bool) {
        // `uucore` holds the exit code in a process-global, so a failing case
        // poisons every later one in the same process. Production never sees
        // this — each applet gets its own process — but the test harness is
        // one process and threaded, so cases run under a lock and reset it.
        let _guard = GOLDEN.lock().unwrap_or_else(|e| e.into_inner());
        uucore::error::set_exit_code(0);
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        setup(dir);
        let cwd = std::env::current_dir().unwrap();
        if chdir {
            std::env::set_current_dir(dir).unwrap();
        }
        // uutils resolves relative operands against the process cwd, and the
        // test harness is threaded, so every case uses absolute paths instead
        // of chdir-ing. `setup` writes under `dir`; `args` are rewritten here.
        let rewritten: Vec<String> = args
            .iter()
            .map(|a| {
                if let Some(rest) = a.strip_prefix('@') {
                    dir.join(rest).to_string_lossy().to_string()
                } else {
                    (*a).to_string()
                }
            })
            .collect();
        let mut argv: Vec<OsString> = Vec::with_capacity(rewritten.len());
        for a in &rewritten {
            argv.push(OsString::from(a));
        }
        let name = rewritten.first().expect("case has a program name").clone();
        let got = dispatch(&name, argv).unwrap_or_else(|| panic!("no applet named {name}"));
        assert_eq!(got, code, "exit code for {args:?}");
        let listing = tree(dir);
        if chdir {
            std::env::set_current_dir(&cwd).unwrap();
        }
        let want: Vec<String> = expect.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(listing, want, "resulting tree for {args:?}");
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    // --- the table ---

    #[test]
    fn applet_names_are_unique_and_sorted() {
        let names: Vec<&str> = APPLETS.iter().map(|a| a.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "APPLETS must be ordered by name");
        let unique: BTreeSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "duplicate applet name");
    }

    #[test]
    fn dispatch_declines_what_we_do_not_ship() {
        assert!(!is_applet("awk"));
        assert!(!is_applet("bash"));
        assert!(!is_applet(""));
        assert!(dispatch("awk", argv(&["awk"])).is_none());
    }

    #[test]
    fn the_headline_divergences_are_covered() {
        // Each of these is a documented GNU/BSD split. If one is dropped from
        // the table the promise quietly stops holding, so name them here.
        for name in [
            "install",
            "cp",
            "mv",
            "ln",
            "stat",
            "readlink",
            "realpath",
            "mktemp",
            "touch",
            "sort",
            "wc",
            "sha256sum",
            "base64",
            "timeout",
            "nproc",
            "date",
        ] {
            assert!(is_applet(name), "{name} must ship");
        }
    }

    // --- golden cases: filesystem effects + exit code ---

    #[test]
    fn install_d_makes_the_parent_dirs() {
        // The single most-hit macOS failure: BSD `install` has no `-D`.
        golden(
            |d| write(d, "src", "hi\n"),
            &["install", "-D", "@src", "@out/x/y/z"],
            0,
            &[
                "out/",
                "out/x/",
                "out/x/y/",
                "out/x/y/z (3 bytes, executable)",
                "src (3 bytes)",
            ],
        );
    }

    #[test]
    fn cp_recursive_takes_lowercase_r() {
        // GNU accepts `-r`; BSD wants `-R`. Ours takes both, and this pins `-r`.
        golden(
            |d| {
                write(d, "a/one", "1\n");
                write(d, "a/sub/two", "22\n");
            },
            &["cp", "-r", "@a", "@b"],
            0,
            &[
                "a/",
                "a/one (2 bytes)",
                "a/sub/",
                "a/sub/two (3 bytes)",
                "b/",
                "b/one (2 bytes)",
                "b/sub/",
                "b/sub/two (3 bytes)",
            ],
        );
    }

    #[test]
    fn cp_parents_keeps_the_path() {
        // `--parents` is GNU-only, and is defined on a relative source path.
        golden_cwd(
            |d| {
                write(d, "a/b/c", "x\n");
                std::fs::create_dir_all(d.join("dst")).unwrap();
            },
            &["cp", "--parents", "a/b/c", "dst"],
            0,
            &[
                "a/",
                "a/b/",
                "a/b/c (2 bytes)",
                "dst/",
                "dst/a/",
                "dst/a/b/",
                "dst/a/b/c (2 bytes)",
            ],
        );
    }

    #[test]
    fn ln_r_makes_a_relative_symlink() {
        // `ln -r` is GNU-only.
        golden(
            |d| write(d, "target", "t\n"),
            &["ln", "-sr", "@target", "@link"],
            0,
            &["link -> target", "target (2 bytes)"],
        );
    }

    #[test]
    fn mkdir_p_is_idempotent() {
        golden(
            |d| {
                std::fs::create_dir_all(d.join("a/b")).unwrap();
            },
            &["mkdir", "-p", "@a/b/c"],
            0,
            &["a/", "a/b/", "a/b/c/"],
        );
    }

    #[test]
    fn rm_rf_on_a_missing_path_succeeds() {
        golden(|_| {}, &["rm", "-rf", "@nope"], 0, &[]);
    }

    #[test]
    fn rm_without_f_on_a_missing_path_fails() {
        golden(|_| {}, &["rm", "@nope"], 1, &[]);
    }

    #[test]
    fn mv_renames() {
        golden(
            |d| write(d, "from", "v\n"),
            &["mv", "@from", "@to"],
            0,
            &["to (2 bytes)"],
        );
    }

    #[test]
    fn chmod_symbolic_mode_applies() {
        golden(
            |d| write(d, "f", "x\n"),
            &["chmod", "+x", "@f"],
            0,
            &["f (2 bytes, executable)"],
        );
    }

    #[test]
    fn touch_creates_an_empty_file() {
        golden(|_| {}, &["touch", "@new"], 0, &["new (0 bytes)"]);
    }

    // --- the shim directory ---

    #[test]
    fn shim_dir_links_every_applet_at_the_binary() {
        let home = tempfile::tempdir().unwrap();
        let exe = home.path().join("heph");
        std::fs::write(&exe, "not really a binary").unwrap();

        let dir = shim_dir(home.path(), &exe).unwrap();
        for applet in APPLETS {
            let link = dir.join(applet.name);
            let md = std::fs::symlink_metadata(&link).unwrap();
            assert!(md.is_symlink(), "{} must be a symlink", applet.name);
            assert_eq!(std::fs::read_link(&link).unwrap(), exe);
        }
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, APPLETS.len(), "exactly one shim per applet");
    }

    #[test]
    fn shim_dir_is_idempotent_and_leaves_no_staging_behind() {
        let home = tempfile::tempdir().unwrap();
        let exe = home.path().join("heph");
        std::fs::write(&exe, "b").unwrap();

        let first = shim_dir(home.path(), &exe).unwrap();
        let second = shim_dir(home.path(), &exe).unwrap();
        assert_eq!(first, second);

        let leftovers: Vec<_> = std::fs::read_dir(home.path().join("coreutils"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "staging dir was not cleaned up");
    }

    #[test]
    fn shim_dir_separates_two_binaries_sharing_a_home() {
        // Two installs sharing a home must not share a shim set — the links
        // point at a specific binary, not at "whichever heph".
        let home = tempfile::tempdir().unwrap();
        let a = home.path().join("a-heph");
        let b = home.path().join("b-heph");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let da = shim_dir(home.path(), &a).unwrap();
        let db = shim_dir(home.path(), &b).unwrap();
        assert_ne!(da, db);
        assert_eq!(std::fs::read_link(da.join("cp")).unwrap(), a);
        assert_eq!(std::fs::read_link(db.join("cp")).unwrap(), b);
    }
}
