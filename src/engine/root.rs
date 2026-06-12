use crate::engine::get_cwd;
use anyhow::anyhow;
use memoize::memoize;
use std::path::{Path, PathBuf};

pub fn get_root() -> anyhow::Result<PathBuf> {
    get_root_inner().map_err(|e| anyhow!(e))
}

#[memoize]
pub fn get_root_inner() -> Result<PathBuf, String> {
    let cwd = get_cwd().map_err(|e| e.to_string())?;
    let mut current = Path::new(&cwd).to_path_buf();

    loop {
        let found = crate::engine::config_yaml::CONFIG_FILE_NAMES
            .iter()
            .any(|name| current.join(name).exists());
        if found {
            return Ok(current);
        }

        match current.parent().map(|p| p.to_path_buf()) {
            Some(parent) => current = parent,
            None => break,
        }
    }

    Err(format!(
        "Could not find {} file in any parent directory",
        crate::engine::config_yaml::CONFIG_FILE_NAMES.join(" or ")
    ))
}
