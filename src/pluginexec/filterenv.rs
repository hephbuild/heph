// Port of heph/plugin/pluginexec/filterenv_unix.go.
//
// Linux caps each argv/envp entry at MAX_ARG_STRLEN bytes and caps the total
// (argv + envp) size at ARG_MAX. Exceeding either makes execve fail with E2BIG.
// We drop overlong entries, then evict the longest entries until the total fits.
//
// See https://www.in-ulm.de/~mascheck/various/argmax/

use std::ffi::OsString;
use std::sync::OnceLock;

const MAX_ARG_STRLEN: usize = 131072;

fn detect_max_args() -> Option<i64> {
    let out = std::process::Command::new("getconf")
        .arg("ARG_MAX")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?.trim();
    s.parse::<i64>().ok()
}

fn max_args() -> Option<i64> {
    static CELL: OnceLock<Option<i64>> = OnceLock::new();
    *CELL.get_or_init(detect_max_args)
}

fn entry_len(k: &str, v: &str) -> usize {
    // "KEY=VALUE" as a single execve string.
    k.len() + 1 + v.len()
}

fn env_byte_length(env: &[(String, String)]) -> i64 {
    let mut l: i64 = 0;
    for (k, v) in env {
        l += entry_len(k, v) as i64;
        l += 2;
    }
    l += 2048;
    l
}

fn remove_longest(env: &mut Vec<(String, String)>) -> bool {
    let Some((i, _)) = env
        .iter()
        .enumerate()
        .max_by_key(|(_, (k, v))| entry_len(k, v))
    else {
        return false;
    };
    env.remove(i);
    true
}

fn filter_impl(env: &mut Vec<(String, String)>, args_len: i64, maxl: i64) {
    env.retain(|(k, v)| entry_len(k, v) <= MAX_ARG_STRLEN);
    while env_byte_length(env) + args_len >= maxl && remove_longest(env) {}
}

pub fn filter_long_env(mut env: Vec<(String, String)>, args: &[OsString]) -> Vec<(String, String)> {
    let Some(maxl) = max_args() else { return env };
    if maxl <= 0 {
        return env;
    }
    let args_len: i64 = args.iter().map(|a| a.len() as i64).sum();
    filter_impl(&mut env, args_len, maxl);
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_overlong_entry() {
        let huge = "x".repeat(MAX_ARG_STRLEN + 10);
        let mut env = vec![
            ("KEEP".to_string(), "value".to_string()),
            ("BIG".to_string(), huge),
        ];
        filter_impl(&mut env, 0, i64::MAX);
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "KEEP");
    }

    #[test]
    fn evicts_longest_until_under_limit() {
        let mut env = vec![
            ("A".to_string(), "x".repeat(100)),
            ("B".to_string(), "x".repeat(500)),
            ("C".to_string(), "x".repeat(200)),
        ];
        // 2860 with all three; 2356 after removing B.
        filter_impl(&mut env, 0, 2500);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["A", "C"]);
    }

    #[test]
    fn args_length_counted() {
        let mut env = vec![("A".to_string(), "x".repeat(100))];
        let baseline = env_byte_length(&env);
        filter_impl(&mut env, 0, baseline + 10);
        assert_eq!(env.len(), 1);

        filter_impl(&mut env, 10_000, baseline + 10);
        assert!(env.is_empty());
    }
}
