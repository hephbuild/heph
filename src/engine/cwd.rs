use std::env;
use std::path::{Path, PathBuf};
use anyhow::anyhow;
use memoize::memoize;
use crate::engine::get_root;
use crate::htpkg::PkgBuf;

pub fn get_cwd() -> anyhow::Result<PathBuf> {
    get_cwd_inner().map_err(|e| anyhow!(e))
}

#[memoize]
pub fn get_cwd_inner() -> Result<PathBuf, String> {
    match env::var_os("HEPH_CWD") {
        Some(v) => Ok(Path::new(&v).to_path_buf()),
        None => Ok(env::current_dir().map_err(|e| e.to_string())?),
    }
}

pub fn get_cwp() -> anyhow::Result<PkgBuf> {
    get_cwp_inner().map_err(|e| anyhow!(e))
}

#[memoize]
pub fn get_cwp_inner() -> Result<PkgBuf, String> {
    let cwd = get_cwd().map_err(|e| e.to_string())?;
    let root = get_root().map_err(|e| e.to_string())?;

    Ok(Path::new(&cwd)
        .strip_prefix(&root)
        .map(|p| PkgBuf::from(p.to_str().unwrap_or("")))
        .unwrap_or_else(|_| PkgBuf::from("")))
}
