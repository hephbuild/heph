use crate::hartifactcontent::Content;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub fn unpack(content: &dyn Content, dst: &Path, list_dst: &Path) -> anyhow::Result<()> {
    let mut list_dst_f = io::BufWriter::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(list_dst)?,
    );

    for entry in content.walk()? {
        let mut entry = entry?;
        let dest = dst.join(&entry.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        list_dst_f.write_all(format!("{}\n", dest.display()).as_bytes())?;
        let mut dest_file = io::BufWriter::with_capacity(65536, fs::File::create(&dest)?);
        io::copy(&mut entry.data, &mut dest_file)?;
        #[cfg(unix)]
        if entry.x {
            dest_file.flush()?;
            let file = dest_file.get_ref();
            let mut perms = file.metadata()?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&dest, perms)?;
        }
    }
    Ok(())
}
