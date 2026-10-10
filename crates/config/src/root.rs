use crate::config_yaml::{CONFIG_FILE_NAME, CONFIG_FILE_NAMES};
use crate::home_dir::normalize;
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
    cwd_from(env::var_os("HEPH_CWD").as_deref())
}

/// The cwd for a `HEPH_CWD` value, made absolute and normalized the same way the
/// home is ([`normalize`]). A relative `HEPH_CWD=.` used to make the workspace
/// root relative — and a `/w/x/../ws` spelled it differently from the home —
/// so the home never matched a walked path and was not pruned.
fn cwd_from(heph_cwd: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    match heph_cwd {
        Some(v) => normalize(Path::new(v))
            .map_err(|e| format!("normalizing HEPH_CWD {}: {e:#}", Path::new(v).display())),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_heph_cwd_is_made_absolute() {
        let cwd = cwd_from(Some(std::ffi::OsStr::new("."))).expect("cwd");
        assert!(cwd.is_absolute(), "{}", cwd.display());
        let sub = cwd_from(Some(std::ffi::OsStr::new("a/b"))).expect("cwd");
        assert!(
            sub.is_absolute() && sub.ends_with("a/b"),
            "{}",
            sub.display()
        );
    }

    /// `HEPH_CWD=/w/x/../ws` with `homeDir: state`: the root and the home are
    /// spelled the same way, so the home is still an exact path under the root —
    /// what the walks' exact-path prune compares.
    #[test]
    fn a_dotdot_heph_cwd_normalizes_like_the_home() {
        let cwd = cwd_from(Some(std::ffi::OsStr::new("/w/x/../ws"))).expect("cwd");
        assert_eq!(cwd, Path::new("/w/ws"));
        let home = crate::HomeDir::resolve(&cwd, Some(Path::new("state"))).expect("home");
        assert_eq!(home.as_path(), cwd.join("state"));
    }
}
