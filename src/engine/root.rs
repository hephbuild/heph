use std::path::Path;
use memoize::memoize;
use crate::engine::get_cwd;

#[memoize]
pub fn get_root() -> Result<String, String> {
    let cwd = get_cwd()?;
    let mut current = Path::new(&cwd);

    loop {
        let config_file = current.join(".hephconfig2");
        if config_file.exists() {
            return Ok(current.to_str().unwrap().to_string());
        }

        match current.parent() {
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
        assert_eq!(result.unwrap(), root_path.to_str().unwrap());

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
