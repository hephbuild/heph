// Port of heph/plugin/pluginexec/filterenv_unix.go.
//
// Linux caps each argv/envp entry at MAX_ARG_STRLEN bytes and caps the total
// (argv + envp) size at ARG_MAX. Exceeding either makes execve fail with E2BIG.
// We drop overlong entries, then evict the longest entries until the total fits.
//
// See https://www.in-ulm.de/~mascheck/various/argmax/

use std::collections::BTreeSet;
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

/// Evict the longest *evictable* entry. Returns false when only protected
/// entries are left.
fn remove_longest(env: &mut Vec<(String, String)>, protected: &BTreeSet<String>) -> bool {
    let Some((i, _)) = env
        .iter()
        .enumerate()
        .filter(|(_, (k, _))| !protected.contains(k))
        .max_by_key(|(_, (k, v))| entry_len(k, v))
    else {
        return false;
    };
    env.remove(i);
    true
}

fn filter_impl(
    env: &mut Vec<(String, String)>,
    args_len: i64,
    maxl: i64,
    protected: &BTreeSet<String>,
) -> anyhow::Result<()> {
    env.retain(|(k, v)| entry_len(k, v) <= MAX_ARG_STRLEN || protected.contains(k));
    while env_byte_length(env) + args_len >= maxl && remove_longest(env, protected) {}
    let mut names: Vec<&str> = env
        .iter()
        .filter(|(k, _)| protected.contains(k))
        .map(|(k, _)| k.as_str())
        .collect();
    // Only a *credential* that cannot fit is worth failing for. With nothing
    // protected left, the pre-existing behaviour stands: evict everything and let
    // `execve` decide, which is what a target with a huge argv has always got.
    if !names.is_empty() && env_byte_length(env) + args_len >= maxl {
        names.sort_unstable();
        anyhow::bail!(
            "this target's command line plus its credentials ({}) is larger than this system's \
             ARG_MAX ({maxl} bytes). Dropping a credential to make room would run the target \
             unauthenticated — or worse, against whatever ambient identity the host has — so the \
             target fails instead. Present the largest values as `files` rather than `env`",
            names.join(", ")
        );
    }
    Ok(())
}

/// Fit `env` under this system's `ARG_MAX`, evicting the longest entries first.
///
/// `protected` names entries the limiter may **never** evict: credential values.
/// Without that exemption the eviction order — longest first — picks precisely a
/// session token, and because `ARG_MAX` differs by operating system the same
/// BUILD file would keep the credential on Linux and drop it on macOS, then run
/// unauthenticated or against whatever ambient identity the host had. That is a
/// silent per-platform divergence in *who the build acts as*, which is the one
/// class of difference this must never produce.
///
/// If the protected set alone does not fit, the target fails rather than
/// spawning.
pub fn filter_long_env(
    mut env: Vec<(String, String)>,
    args: &[OsString],
    protected: &BTreeSet<String>,
) -> anyhow::Result<Vec<(String, String)>> {
    let Some(maxl) = max_args() else {
        return Ok(env);
    };
    if maxl <= 0 {
        return Ok(env);
    }
    let args_len: i64 = args.iter().map(|a| a.len() as i64).sum();
    filter_impl(&mut env, args_len, maxl, protected)?;
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn protect(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn drops_overlong_entry() {
        let huge = "x".repeat(MAX_ARG_STRLEN + 10);
        let mut env = vec![
            ("KEEP".to_string(), "value".to_string()),
            ("BIG".to_string(), huge),
        ];
        filter_impl(&mut env, 0, i64::MAX, &none()).expect("fits");
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
        filter_impl(&mut env, 0, 2500, &none()).expect("fits");
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["A", "C"]);
    }

    #[test]
    fn args_length_counted() {
        let mut env = vec![("A".to_string(), "x".repeat(100))];
        let baseline = env_byte_length(&env);
        filter_impl(&mut env, 0, baseline + 10, &none()).expect("fits");
        assert_eq!(env.len(), 1);

        filter_impl(&mut env, 10_000, baseline + 10, &none()).expect("fits");
        assert!(env.is_empty());
    }

    /// The eviction order is longest-first, which is precisely a session token.
    #[test]
    fn a_credential_is_never_the_entry_that_gets_evicted() {
        let mut env = vec![
            ("AWS_SESSION_TOKEN".to_string(), "x".repeat(500)),
            ("BULK".to_string(), "x".repeat(200)),
        ];
        filter_impl(&mut env, 0, 2600, &protect(&["AWS_SESSION_TOKEN"])).expect("fits");
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["AWS_SESSION_TOKEN"]);
    }

    /// If only the protected set is left and it still does not fit, spawning
    /// would run the target unauthenticated. Fail instead.
    #[test]
    fn a_credential_that_cannot_fit_fails_the_target_rather_than_being_dropped() {
        let mut env = vec![("AWS_SESSION_TOKEN".to_string(), "x".repeat(500))];
        let err =
            filter_impl(&mut env, 0, 100, &protect(&["AWS_SESSION_TOKEN"])).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("AWS_SESSION_TOKEN"), "{msg}");
        assert!(msg.contains("unauthenticated"), "{msg}");
    }

    /// `MAX_ARG_STRLEN` is a per-entry cap, and dropping a protected entry for it
    /// would be the same silent de-authentication by another route. Keeping it
    /// makes `execve` fail with `E2BIG`, which is a visible failure rather than a
    /// build that quietly ran as nobody.
    #[test]
    fn an_overlong_credential_is_kept_so_the_spawn_fails_visibly() {
        let mut env = vec![("TOKEN".to_string(), "x".repeat(MAX_ARG_STRLEN + 10))];
        filter_impl(&mut env, 0, i64::MAX, &protect(&["TOKEN"])).expect("no total-size failure");
        assert_eq!(env.len(), 1, "a credential is never silently dropped");
    }

    /// The pre-credential behaviour is untouched: a target whose argv alone
    /// blows past ARG_MAX still gets an empty env and a spawn attempt, not a new
    /// hard failure.
    #[test]
    fn a_target_with_no_credentials_still_just_gets_everything_evicted() {
        let mut env = vec![("A".to_string(), "x".repeat(100))];
        filter_impl(&mut env, 10_000, 2500, &none()).expect("no failure without credentials");
        assert!(env.is_empty());
    }
}
