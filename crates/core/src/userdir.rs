//! The per-user heph directory, `~/.heph`.
//!
//! Holds what belongs to the user rather than to any one workspace: the plugin
//! download cache (`~/.heph/plugins/`) and the live session records
//! (`~/.heph/sessions/`). One lookup, so the two can never disagree on where
//! "the user's heph directory" is.
//!
//! The caller decides *when* the environment is read: [`heph_user_dir`] reads
//! `HOME` now, [`heph_user_dir_from`] takes a value the caller already read
//! (a front end that reads its environment once, or a test).

use std::ffi::OsStr;
use std::path::PathBuf;

/// The directory's name under `$HOME`.
pub const USER_DIR_NAME: &str = ".heph";

/// `$HOME/.heph`, reading `HOME` from the environment.
pub fn heph_user_dir() -> anyhow::Result<PathBuf> {
    heph_user_dir_from(std::env::var_os("HOME").as_deref())
}

/// `<home>/.heph` for an already-read `HOME`. Unset and empty are the same
/// error: an empty `HOME` would put the directory relative to the cwd, which is
/// a different directory in every workspace.
pub fn heph_user_dir_from(home: Option<&OsStr>) -> anyhow::Result<PathBuf> {
    match home {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h).join(USER_DIR_NAME)),
        _ => anyhow::bail!("HOME is not set; cannot locate ~/{USER_DIR_NAME}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_home_and_refuses_unset_or_empty() {
        assert_eq!(
            heph_user_dir_from(Some(OsStr::new("/u/me"))).unwrap(),
            PathBuf::from("/u/me/.heph")
        );
        assert!(heph_user_dir_from(None).is_err());
        assert!(heph_user_dir_from(Some(OsStr::new(""))).is_err());
    }
}
