use std::fs;
use std::io;
use std::path::Path;
use crate::hartifactcontent::Content;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub fn unpack(content: &dyn Content, to: &Path) -> anyhow::Result<()> {
    for entry in content.walk()? {
        let mut entry = entry?;
        let dest = to.join(&entry.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut dest_file = fs::File::create(&dest)?;
        io::copy(&mut entry.data, &mut dest_file)?;
        #[cfg(unix)]
        if entry.x {
            let mut perms = dest_file.metadata()?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&dest, perms)?;
        }
    }
    Ok(())
}
