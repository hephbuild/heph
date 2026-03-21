use std::env;
use memoize::memoize;

#[memoize]
pub fn get_cwd() -> Result<String, String> {
    match env::var_os("HEPH_CWD") {
        Some(v) => Ok(v.to_str().unwrap().to_string()),
        None => Ok(env::current_dir().map_err(|e| e.to_string())?.to_str().unwrap().to_string()),
    }
}