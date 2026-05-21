use crate::hartifactcontent::Content;
use anyhow::Context;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
    list_dst: &Path,
    should_unpack: Option<&dyn Fn(&Path) -> bool>,
) -> anyhow::Result<()> {
    let mut list_dst_f = io::BufWriter::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(list_dst)
            .with_context(|| format!("open list file {:?} (append)", list_dst))?,
    );

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
        list_dst_f
            .write_all(format!("{}\n", dest.display()).as_bytes())
            .with_context(|| format!("append to list file {:?}", list_dst))?;
        let f = fs::File::create(&dest).with_context(|| {
            format!(
                "create unpack dest {:?} (entry={:?}, existing={})",
                dest,
                entry.path,
                describe_existing(&dest),
            )
        })?;
        let mut dest_file = io::BufWriter::with_capacity(65536, f);
        io::copy(&mut entry.data, &mut dest_file)
            .with_context(|| format!("copy entry data into {:?}", dest))?;
        #[cfg(unix)]
        if entry.x {
            dest_file
                .flush()
                .with_context(|| format!("flush {:?} before chmod", dest))?;
            let file = dest_file.get_ref();
            let mut perms = file
                .metadata()
                .with_context(|| format!("stat {:?} for chmod", dest))?
                .permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&dest, perms).with_context(|| format!("chmod +x {:?}", dest))?;
        }
    }
    Ok(())
}
