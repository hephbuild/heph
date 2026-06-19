use anyhow::anyhow;
use hconfig::{get_cwd, get_root};
use hmodel::htpkg::PkgBuf;
use memoize::memoize;
use std::path::Path;

/// Current working *package*: the cwd expressed relative to the workspace root.
/// Empty when cwd is the root itself or lies outside it.
pub fn get_cwp() -> anyhow::Result<PkgBuf> {
    get_cwp_inner().map_err(|e| anyhow!(e))
}

#[memoize]
fn get_cwp_inner() -> Result<PkgBuf, String> {
    let cwd = get_cwd().map_err(|e| e.to_string())?;
    let root = get_root().map_err(|e| e.to_string())?;

    Ok(Path::new(&cwd)
        .strip_prefix(&root)
        .map(|p| PkgBuf::from(p.to_str().unwrap_or("")))
        .unwrap_or_else(|_| PkgBuf::from("")))
}
