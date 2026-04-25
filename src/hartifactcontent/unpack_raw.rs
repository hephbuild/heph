use std::fs;
use std::path::Path;
use crate::hartifactcontent::ContentRaw;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub fn unpack_raw(raw: &ContentRaw, to: &Path) -> anyhow::Result<()> {
    let dest = to.join(&raw.path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, &raw.data)?;
    #[cfg(unix)]
    if raw.x {
        let mut perms = fs::metadata(&dest)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(&dest, perms)?;
    }
    Ok(())
}
