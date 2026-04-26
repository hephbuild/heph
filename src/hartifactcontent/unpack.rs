use std::fs;
use std::path::Path;
use crate::hartifactcontent::{Artifact};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub fn unpack(content: &dyn Artifact, to: &Path) -> anyhow::Result<()> {
    for entry in content.walk()? {
        let entry = entry?;
        let dest = to.join(&entry.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, &entry.data)?;
        #[cfg(unix)]
        if entry.x {
            let mut perms = fs::metadata(&dest)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&dest, perms)?;
        }
    }
    Ok(())
}
