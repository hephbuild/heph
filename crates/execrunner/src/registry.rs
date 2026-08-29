//! The runner registry: named implementations, and how one gets picked.
//!
//! Two identifiers do two different jobs. A consumer names an **address**
//! (`runner = "//tools/devenv:runner"`), which is what reaches the cache key —
//! only a target has a hashout. The `runner.json` at that address names an
//! **implementation** (`"runner": "devenv"`), which is what selects code.
//!
//! ```text
//! target        runner = "//tools/devenv:runner"   ← an ADDRESS
//!    ↓          hashed input, built before hashin
//! runner.json   { "runner": "devenv", "config": {…} }   ← a NAME
//!    ↓          registry lookup
//! plugin        whichever component registered "devenv"
//! ```
//!
//! The builtin `local` and `wrap` runners are registered here like any other,
//! so there is no mode discriminant beside the name and a third-party runner is
//! a peer of the builtins rather than a special case.

use crate::SpecRewrite;
use crate::config::{RunnerConfig, WrapConfig};
use hcore::hasync::Cancellable;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;

/// What a runner is told about the exec it is rewriting.
pub struct RunnerCtx<'a> {
    /// Canonical address of the runner target, for diagnostics.
    pub addr: &'a str,
    /// The environment digest the runner target declared.
    pub fingerprint: &'a str,
    /// The runner-specific half of `runner.json`, unvalidated.
    pub config: &'a serde_json::Value,
    /// Cancellation for anything slow (an agent runner starting a session).
    pub ctoken: &'a (dyn Cancellable + Send + Sync),
}

/// A named way of running a process somewhere other than plainly here.
#[async_trait::async_trait]
pub trait ExecRunner: Send + Sync {
    /// The name a `runner.json` selects this implementation by.
    fn name(&self) -> &str;

    /// Rewrite the spec. May be slow on first use if it has a session to start.
    async fn prepare(
        &self,
        ctx: &RunnerCtx<'_>,
        rewrite: SpecRewrite,
    ) -> anyhow::Result<SpecRewrite>;

    /// Whether this runner puts the target into an environment of its own.
    ///
    /// Decides one thing: whether the driver's fallback `PATH` may be reinstated
    /// when neither the target nor the runner produced one here (see
    /// [`crate::PathPolicy`]). An agent runner answers yes — its environment is
    /// the agent's `environ`, which this process cannot see, so reinstating a
    /// fallback would put the driver's `/usr/bin` ahead of the environment the
    /// target asked to run in and let a host-installed tool shadow it.
    fn supplies_environment(&self) -> bool {
        false
    }

    /// Release anything held open — a session, a container.
    ///
    /// Synchronous, because the only caller is `Engine`'s `Drop`. It cannot be
    /// left to the runner's own `Drop`: the registry is reachable from a
    /// process-global (the installed host), and Rust never runs destructors for
    /// statics. A runner relying on `Drop` therefore leaks its processes on
    /// every exit — and a leaked agent that inherited a descriptor can hang
    /// whatever is reading it.
    ///
    /// Best-effort and idempotent: it runs during teardown, where there is
    /// nowhere to report an error to.
    fn shutdown(&self) {}
}

/// Every runner this host knows, by name.
///
/// Name collisions are a hard error at registration, matching how the engine
/// treats drivers (`insert_driver`) and providers (`try_register_provider`) —
/// all three are looked up by name, unlike hooks, which are only ever fanned
/// out to and so need no uniqueness guard.
#[derive(Default)]
pub struct RunnerRegistry {
    by_name: BTreeMap<String, Arc<dyn ExecRunner>>,
}

impl RunnerRegistry {
    /// A registry with the builtins (`local`, `wrap`) already in it.
    pub fn with_builtins() -> Self {
        let mut r = Self::default();
        // Inserted directly rather than through `register`: the map starts
        // empty and the two builtin names differ, so there is no collision to
        // check for, and `register`'s `Result` would only be discarded. A third
        // builtin shadowing one of these is caught by
        // `the_builtins_are_all_registered` below.
        r.by_name.insert("local".to_string(), Arc::new(LocalRunner));
        r.by_name.insert("wrap".to_string(), Arc::new(WrapRunner));
        r
    }

    pub fn register(&mut self, runner: Arc<dyn ExecRunner>) -> anyhow::Result<()> {
        let name = runner.name().to_string();
        if self.by_name.contains_key(&name) {
            anyhow::bail!(
                "exec runner '{name}' is registered twice. Runner names are looked up by \
                 `runner.json`, so two components claiming one name would make which \
                 implementation runs a target depend on load order."
            );
        }
        self.by_name.insert(name, runner);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn ExecRunner>> {
        self.by_name.get(name)
    }

    /// Tear down every runner. See [`ExecRunner::shutdown`].
    pub fn shutdown_all(&self) {
        for runner in self.by_name.values() {
            runner.shutdown();
        }
    }

    /// Registered names, sorted. Surfaced in the unknown-runner diagnostic and
    /// by `heph inspect`.
    pub fn names(&self) -> Vec<&str> {
        self.by_name.keys().map(String::as_str).collect()
    }

    /// Validate a parsed config against this registry.
    ///
    /// Called at *resolution* time — as soon as the runner target's output is
    /// available, which is before `hashin` exists and therefore before any
    /// consumer executes. Validating at spawn instead would surface a typo as a
    /// mid-build failure on an arbitrary target.
    pub fn validate(&self, addr: &str, cfg: &RunnerConfig) -> anyhow::Result<()> {
        if self.get(&cfg.runner).is_none() {
            anyhow::bail!(
                "runner {addr}: no exec runner named '{}' is registered. Known runners: {}. \
                 A runner shipped by a plugin is only available once that plugin is loaded — \
                 check the `plugins:` list in .hephconfig2.",
                cfg.runner,
                self.names().join(", ")
            );
        }
        Ok(())
    }

    /// Dispatch a rewrite to the named runner.
    pub async fn prepare(
        &self,
        addr: &str,
        cfg: &RunnerConfig,
        rewrite: SpecRewrite,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<crate::PrepareOutcome> {
        self.validate(addr, cfg)?;
        let runner = self
            .get(&cfg.runner)
            .ok_or_else(|| anyhow::anyhow!("runner {addr}: '{}' vanished", cfg.runner))?;
        let ctx = RunnerCtx {
            addr,
            fingerprint: &cfg.fingerprint,
            config: &cfg.config,
            ctoken,
        };
        Ok(crate::PrepareOutcome {
            rewrite: runner.prepare(&ctx, rewrite).await?,
            supplies_environment: runner.supplies_environment(),
        })
    }
}

/// The builtin identity runner.
///
/// Reachable by name for a runner target that deliberately opts out (a
/// per-package override of a workspace-wide default). The common local path
/// never gets here — it short-circuits in `prepare` before any resolution.
struct LocalRunner;

#[async_trait::async_trait]
impl ExecRunner for LocalRunner {
    fn name(&self) -> &str {
        "local"
    }

    async fn prepare(
        &self,
        _ctx: &RunnerCtx<'_>,
        rewrite: SpecRewrite,
    ) -> anyhow::Result<SpecRewrite> {
        Ok(rewrite)
    }
}

/// The builtin static-rewrite runner: argv prefix plus environment.
struct WrapRunner;

#[async_trait::async_trait]
impl ExecRunner for WrapRunner {
    fn name(&self) -> &str {
        "wrap"
    }

    async fn prepare(
        &self,
        ctx: &RunnerCtx<'_>,
        mut rewrite: SpecRewrite,
    ) -> anyhow::Result<SpecRewrite> {
        let cfg: WrapConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| anyhow::anyhow!("runner {}: parse wrap config: {e}", ctx.addr))?;

        if !cfg.prefix.is_empty() {
            // The prefix's head becomes the program; the original program
            // becomes its first argument. `program` is not merely argv[0] —
            // it is the path `execve` resolves — so it has to move.
            let mut argv: Vec<OsString> = Vec::with_capacity(cfg.prefix.len() + rewrite.args.len());
            for p in cfg.prefix.iter().skip(1) {
                argv.push(OsString::from(p));
            }
            argv.push(rewrite.program.clone().into_os_string());
            argv.append(&mut rewrite.args);
            rewrite.args = argv;
            let head = cfg
                .prefix
                .first()
                .ok_or_else(|| anyhow::anyhow!("runner {}: empty wrap prefix", ctx.addr))?;
            rewrite.program = std::path::PathBuf::from(head);
        }

        // Hashed environment from the config wins over the target's own: the
        // point of a runner is that it decides the environment, and its value
        // is in the cache key while the target's ambient one may not be.
        for (k, v) in &cfg.env {
            set_env(&mut rewrite.env, OsString::from(k), OsString::from(v));
        }

        // Unhashed pull-through, applied last and only when the host actually
        // has the variable. Never overwrites something the config set.
        for name in &cfg.runtime_pass_env {
            if cfg.env.contains_key(name) {
                continue;
            }
            if let Some(v) = std::env::var_os(name) {
                set_env(&mut rewrite.env, OsString::from(name), v);
            }
        }

        Ok(rewrite)
    }
}

/// Set or replace a variable in a cleared-env list, preserving position on
/// replace so the resulting order is stable across runs.
pub(crate) fn set_env(env: &mut Vec<(OsString, OsString)>, key: OsString, value: OsString) {
    if let Some(slot) = env.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        env.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use std::path::PathBuf;

    /// Set or clear a process env var for a test.
    ///
    /// Behind a helper so the `unsafe` sits in one place with one safety
    /// comment, rather than as bare statements the safety-comment lints
    /// disagree about at the tail of a function.
    fn set_test_env(key: &str, value: Option<&str>) {
        match value {
            // SAFETY: the test process is single-threaded with respect to the
            // environment — no other test in this crate reads or writes it.
            Some(v) => unsafe { std::env::set_var(key, v) },
            // SAFETY: as above.
            None => unsafe { std::env::remove_var(key) },
        }
    }

    fn rewrite() -> SpecRewrite {
        SpecRewrite {
            program: PathBuf::from("/bin/echo"),
            args: vec![OsString::from("hi")],
            env: vec![(OsString::from("A"), OsString::from("1"))],
            cwd: PathBuf::from("/sandbox"),
        }
    }

    fn cfg(runner: &str, config: serde_json::Value) -> RunnerConfig {
        RunnerConfig {
            version: 1,
            fingerprint: "fp".to_string(),
            runner: runner.to_string(),
            config,
        }
    }

    async fn run(c: &RunnerConfig, r: SpecRewrite) -> anyhow::Result<SpecRewrite> {
        let reg = RunnerRegistry::with_builtins();
        let ctoken = StdCancellationToken::new();
        reg.prepare("//x:r", c, r, &ctoken)
            .await
            .map(|outcome| outcome.rewrite)
    }

    #[tokio::test]
    async fn local_is_the_identity() {
        let out = run(&cfg("local", serde_json::Value::Null), rewrite())
            .await
            .expect("local");
        assert_eq!(out.program, PathBuf::from("/bin/echo"));
        assert_eq!(out.args, vec![OsString::from("hi")]);
    }

    /// The prefix's head must become `program`, not just argv[0] — `program` is
    /// the path `execve` resolves, so leaving it pointing at the target's
    /// command would run the command and ignore the wrapper entirely.
    #[tokio::test]
    async fn wrap_moves_the_program_into_argv() {
        let out = run(
            &cfg(
                "wrap",
                serde_json::json!({"prefix": ["/usr/bin/devenv", "shell", "--"]}),
            ),
            rewrite(),
        )
        .await
        .expect("wrap");
        assert_eq!(out.program, PathBuf::from("/usr/bin/devenv"));
        assert_eq!(
            out.args,
            vec![
                OsString::from("shell"),
                OsString::from("--"),
                OsString::from("/bin/echo"),
                OsString::from("hi"),
            ]
        );
    }

    #[tokio::test]
    async fn wrap_env_overrides_the_targets_own() {
        let out = run(
            &cfg("wrap", serde_json::json!({"env": {"A": "2", "B": "3"}})),
            rewrite(),
        )
        .await
        .expect("wrap");
        assert_eq!(
            out.env,
            vec![
                (OsString::from("A"), OsString::from("2")),
                (OsString::from("B"), OsString::from("3")),
            ]
        );
    }

    /// The unhashed pull-through must never overwrite the hashed value, or the
    /// cache key would describe an environment the target did not get.
    #[tokio::test]
    async fn runtime_pass_env_never_outranks_the_hashed_env() {
        set_test_env("XR_TEST_COLLIDE", Some("from-host"));
        let out = run(
            &cfg(
                "wrap",
                serde_json::json!({
                    "env": {"XR_TEST_COLLIDE": "from-config"},
                    "runtime_pass_env": ["XR_TEST_COLLIDE"]
                }),
            ),
            rewrite(),
        )
        .await
        .expect("wrap");
        let got = out
            .env
            .iter()
            .find(|(k, _)| k == "XR_TEST_COLLIDE")
            .map(|(_, v)| v.clone());
        assert_eq!(got, Some(OsString::from("from-config")));
        set_test_env("XR_TEST_COLLIDE", None);
    }

    #[tokio::test]
    async fn an_absent_runtime_pass_env_var_is_simply_not_set() {
        let out = run(
            &cfg(
                "wrap",
                serde_json::json!({"runtime_pass_env": ["XR_DEFINITELY_UNSET_9f2c"]}),
            ),
            rewrite(),
        )
        .await
        .expect("wrap");
        assert!(!out.env.iter().any(|(k, _)| k == "XR_DEFINITELY_UNSET_9f2c"));
    }

    #[tokio::test]
    async fn an_unknown_runner_name_lists_the_known_ones() {
        let err = run(&cfg("devnev", serde_json::Value::Null), rewrite())
            .await
            .expect_err("typo must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("'devnev'"), "{msg}");
        assert!(msg.contains("local"), "{msg}");
        assert!(msg.contains("wrap"), "{msg}");
    }

    /// `with_builtins` inserts directly, so this is what catches a third
    /// builtin quietly shadowing one of these two.
    #[test]
    fn the_builtins_are_all_registered() {
        let reg = RunnerRegistry::with_builtins();
        assert_eq!(reg.names(), vec!["local", "wrap"]);
    }

    #[test]
    fn a_duplicate_runner_name_is_rejected() {
        struct Dup;
        #[async_trait::async_trait]
        impl ExecRunner for Dup {
            fn name(&self) -> &str {
                "wrap"
            }
            async fn prepare(
                &self,
                _c: &RunnerCtx<'_>,
                r: SpecRewrite,
            ) -> anyhow::Result<SpecRewrite> {
                Ok(r)
            }
        }
        let mut reg = RunnerRegistry::with_builtins();
        let err = reg.register(Arc::new(Dup)).expect_err("duplicate");
        assert!(format!("{err:#}").contains("registered twice"), "{err:#}");
    }
}
