use std::fs::File;
use std::path::Path;

pub fn unpack_tar(path: &str, to: &Path) -> anyhow::Result<()> {
    let file = File::open(path)?;
    let mut archive = tar::Archive::new(file);
    archive.unpack(to)?;
    Ok(())
}
