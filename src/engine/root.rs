use std::path::{Path, PathBuf};
use anyhow::anyhow;
use memoize::memoize;
use crate::engine::get_cwd;

pub fn get_root() -> anyhow::Result<PathBuf> {
    get_root_inner().map_err(|e| anyhow!(e))
}

#[memoize]
pub fn get_root_inner() -> Result<PathBuf, String> {
    let cwd = get_cwd().map_err(|e| e.to_string())?;
    let mut current = Path::new(&cwd).to_path_buf();

    loop {
        let config_file = current.join(".hephconfig2");
        if config_file.exists() {
            return Ok(current);
        }

        match current.parent().map(|p| p.to_path_buf()) {
            Some(parent) => current = parent,
            None => break,
        }
    }

    Err("Could not find .hephconfig2 file in any parent directory".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use std::env;

    #[test]
    fn test_get_root_finds_config() {
        let dir = tempdir().unwrap();
        let root_path = dir.path();
        let sub_dir = root_path.join("a/b/c");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(root_path.join(".hephconfig2"), "").unwrap();

        // Use HEPH_CWD to mock the current working directory for get_cwd
        unsafe {
            env::set_var("HEPH_CWD", sub_dir.to_str().unwrap());
        }

        let result = get_root();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().to_str().unwrap(), root_path.to_str().unwrap());

        unsafe {
            env::remove_var("HEPH_CWD");
        }
    }

    #[test]
    fn test_get_root_fails_if_no_config() {
        let dir = tempdir().unwrap();
        let sub_dir = dir.path().join("a/b/c");
        fs::create_dir_all(&sub_dir).unwrap();

        unsafe {
            env::set_var("HEPH_CWD", sub_dir.to_str().unwrap());
        }

        // Since we are in a temp dir, it shouldn't find .hephconfig2 up to the root
        let result = get_root();
        assert!(result.is_err());

        unsafe {
            env::remove_var("HEPH_CWD");
        }
    }
}
