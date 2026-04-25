use std::fs;
use std::path::Path;
use crate::hartifactcontent::ContentFile;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub fn unpack_file(file: &ContentFile, to: &Path) -> anyhow::Result<()> {
    let dest = to.join(&file.out_path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&file.source_path, &dest)?;
    #[cfg(unix)]
    if file.x {
        let mut perms = fs::metadata(&dest)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(&dest, perms)?;
    }
    Ok(())
}
