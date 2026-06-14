use crate::hartifactcontent::{Content, WalkEntryKind};
use anyhow::Context;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Close `file` (a freshly-written, writable handle to `path`) in a way that
/// guarantees no writable descriptor for the file survives, then return.
///
/// Works around <https://github.com/rust-lang/rust/issues/114554>: if another
/// thread `fork`s between our `File::create` and a later `exec` of the unpacked
/// binary, the child inherits our writable fd and the `exec` fails with
/// `ETXTBSY`. `flock` locks are tied to the open file description and shared
/// with any forked child, so:
///
/// 1. take an exclusive lock on the writable fd,
/// 2. close it (the lock lives on in any inherited copy),
/// 3. reopen read-only and take a shared lock — this blocks until every
///    writable fd (ours and any forked child's) is gone.
///
/// Only meaningful for files that will be executed, so callers gate on `+x`.
#[cfg(unix)]
fn close_ensure_ro_fd(file: fs::File, path: &Path) -> anyhow::Result<()> {
    file.lock()
        .with_context(|| format!("flock(exclusive) writable fd {:?}", path))?;
    drop(file);

    let ro = fs::File::open(path).with_context(|| format!("reopen {:?} read-only", path))?;
    ro.lock_shared()
        .with_context(|| format!("flock(shared) read-only fd {:?}", path))?;
    drop(ro);
    Ok(())
}

/// Describe what currently exists at `path` for error messages. Returns a
/// short tag like "file", "dir", "symlink->/foo", or "<none>". Never errors.
fn describe_existing(path: &Path) -> String {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => "<none>".to_string(),
        Err(e) => format!("<stat-err: {e}>"),
        Ok(md) => {
            let ft = md.file_type();
            if ft.is_symlink() {
                match fs::read_link(path) {
                    Ok(t) => format!("symlink->{}", t.display()),
                    Err(_) => "symlink->?".to_string(),
                }
            } else if ft.is_dir() {
                "dir".to_string()
            } else if ft.is_file() {
                format!("file({} bytes)", md.len())
            } else {
                format!("{:?}", ft)
            }
        }
    }
}

pub fn unpack(
    content: &dyn Content,
    dst: &Path,
    list_dst: Option<&Path>,
    should_unpack: Option<&dyn Fn(&Path) -> bool>,
) -> anyhow::Result<()> {
    let mut list_dst_f = match list_dst {
        Some(p) => Some(io::BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .with_context(|| format!("open list file {:?} (append)", p))?,
        )),
        None => None,
    };

    for entry in content
        .walk()
        .with_context(|| format!("walk content for unpack into {:?}", dst))?
    {
        let mut entry =
            entry.with_context(|| format!("read entry while unpacking into {:?}", dst))?;
        if let Some(pred) = should_unpack
            && !pred(&entry.path)
        {
            continue;
        }
        let dest = dst.join(&entry.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "create parent dir {:?} for unpack entry {:?} (existing={})",
                    parent,
                    entry.path,
                    describe_existing(parent),
                )
            })?;
        }
        if let Some(ref mut list_f) = list_dst_f {
            list_f
                .write_all(format!("{}\n", dest.display()).as_bytes())
                .with_context(|| {
                    format!("append to list file {:?}", list_dst.expect("Some checked"))
                })?;
        }
        match &mut entry.kind {
            WalkEntryKind::Symlink { target } => {
                if target.is_absolute() {
                    anyhow::bail!(
                        "absolute symlink not allowed when unpacking: {:?} -> {:?}",
                        entry.path,
                        target,
                    );
                }
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&*target, &dest).with_context(|| {
                        format!(
                            "create symlink {:?} -> {:?} (existing={})",
                            dest,
                            target,
                            describe_existing(&dest),
                        )
                    })?;
                }
                #[cfg(not(unix))]
                {
                    anyhow::bail!(
                        "symlink unpack not supported on this platform: {:?} -> {:?}",
                        entry.path,
                        target,
                    );
                }
            }
            WalkEntryKind::File { data, x } => {
                let f = fs::File::create(&dest).with_context(|| {
                    format!(
                        "create unpack dest {:?} (entry={:?}, existing={})",
                        dest,
                        entry.path,
                        describe_existing(&dest),
                    )
                })?;
                let mut dest_file = io::BufWriter::with_capacity(65536, f);
                io::copy(data, &mut dest_file)
                    .with_context(|| format!("copy entry data into {:?}", dest))?;
                #[cfg(unix)]
                if *x {
                    dest_file
                        .flush()
                        .with_context(|| format!("flush {:?} before chmod", dest))?;
                    let file = dest_file
                        .into_inner()
                        .with_context(|| format!("unwrap writer for {:?}", dest))?;
                    let mut perms = file
                        .metadata()
                        .with_context(|| format!("stat {:?} for chmod", dest))?
                        .permissions();
                    perms.set_mode(perms.mode() | 0o111);
                    file.set_permissions(perms)
                        .with_context(|| format!("chmod +x {:?}", dest))?;
                    // Executables may be exec'd immediately after unpack; ensure no
                    // writable fd survives to avoid ETXTBSY (rust-lang/rust#114554).
                    close_ensure_ro_fd(file, &dest)
                        .with_context(|| format!("ensure read-only fd for {:?}", dest))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hartifactcontent::tar::{TarPacker, TarWalker};
    use crate::hartifactcontent::{Content, WalkEntry};
    use std::io::{Cursor, Read};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct TarBytes(Vec<u8>);

    impl Content for TarBytes {
        fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
            Ok(Box::new(Cursor::new(self.0.clone())))
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(TarWalker::new(Cursor::new(self.0.clone()))?))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn pack_with_symlink(absolute: bool) -> Vec<u8> {
        let src = tempfile::tempdir().expect("src tempdir");
        let target = src.path().join("target.txt");
        std::fs::write(&target, b"hello").unwrap();
        let link = src.path().join("link.txt");
        if absolute {
            let mut buf = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut buf);
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                builder
                    .append_link(&mut header, "link.txt", "/pathshouldnotexist")
                    .unwrap();
                builder.finish().unwrap();
            }
            return buf;
        }
        symlink("target.txt", &link).unwrap();
        let mut packer = TarPacker::new();
        packer.create_file(target.to_str().unwrap(), "target.txt");
        packer.create_file(link.to_str().unwrap(), "link.txt");
        let mut buf = Vec::new();
        packer.pack(&mut buf).unwrap();
        buf
    }

    #[test]
    fn unpack_relative_symlink() {
        let buf = pack_with_symlink(false);
        let content = TarBytes(buf);
        let out = tempfile::tempdir().expect("out tempdir");
        unpack(&content, out.path(), None, None).expect("unpack");

        let link_path = out.path().join("link.txt");
        let md = std::fs::symlink_metadata(&link_path).unwrap();
        assert!(md.file_type().is_symlink(), "link.txt must be symlink");
        let read_target = std::fs::read_link(&link_path).unwrap();
        assert_eq!(read_target, PathBuf::from("target.txt"));

        // Symlink resolves to file with the expected content.
        let resolved = std::fs::read(&link_path).unwrap();
        assert_eq!(resolved, b"hello");
    }

    #[test]
    fn unpack_executable_is_runnable() {
        // A +x entry must come out executable, fully closed (no lingering
        // writable fd), and immediately exec'able — see close_ensure_ro_fd /
        // rust-lang/rust#114554.
        let mut packer = TarPacker::new();
        packer.create_raw(b"#!/bin/sh\necho ok\n".to_vec(), "script.sh", true);
        let mut buf = Vec::new();
        packer.pack(&mut buf).unwrap();
        let content = TarBytes(buf);

        let out = tempfile::tempdir().expect("out tempdir");
        unpack(&content, out.path(), None, None).expect("unpack");

        let script = out.path().join("script.sh");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "script must be executable, mode={mode:o}"
        );

        let output = std::process::Command::new(&script)
            .output()
            .expect("exec unpacked script");
        assert!(
            output.status.success(),
            "script exited non-zero: {output:?}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
    }

    #[test]
    fn unpack_absolute_symlink_errors() {
        let buf = pack_with_symlink(true);
        let content = TarBytes(buf);
        let out = tempfile::tempdir().expect("out tempdir");
        let err = unpack(&content, out.path(), None, None).expect_err("expected error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute symlink not allowed"),
            "unexpected: {msg}"
        );
    }
}
