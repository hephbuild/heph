use crate::config_yaml::{CONFIG_FILE_NAME, CONFIG_FILE_NAMES};
use anyhow::anyhow;
use memoize::memoize;
use std::env;
use std::path::{Path, PathBuf};

/// Current working directory heph operates from. Honors `HEPH_CWD` (used by tests
/// and tooling to point heph at another tree) before falling back to the real
/// process cwd.
pub fn get_cwd() -> anyhow::Result<PathBuf> {
    get_cwd_inner().map_err(|e| anyhow!(e))
}

#[memoize]
fn get_cwd_inner() -> Result<PathBuf, String> {
    match env::var_os("HEPH_CWD") {
        Some(v) => Ok(Path::new(&v).to_path_buf()),
        None => env::current_dir().map_err(|e| e.to_string()),
    }
}

/// Walk up from [`get_cwd`] until a workspace config file is found, returning the
/// directory that holds it (the workspace root). Errors if none is found in any
/// parent.
pub fn get_root() -> anyhow::Result<PathBuf> {
    get_root_inner().map_err(|e| anyhow!(e))
}

#[memoize]
fn get_root_inner() -> Result<PathBuf, String> {
    let cwd = get_cwd().map_err(|e| e.to_string())?;
    let mut current = Path::new(&cwd).to_path_buf();

    loop {
        let found = CONFIG_FILE_NAMES
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
        "Could not find {CONFIG_FILE_NAME} file in any parent directory"
    ))
}
