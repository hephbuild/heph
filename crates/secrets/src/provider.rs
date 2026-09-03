//! Broker providers: the code that actually obtains a value.
//!
//! Providers register by name in the same registry style as exec runners, so a
//! third-party provider is a peer of the builtins rather than a special case.
//!
//! - [`StaticEnvProvider`] — reads a named host variable. The honest escape
//!   hatch, and the migration path off `pass_env`.
//! - [`ExecProvider`] — a helper subprocess speaking one of the four
//!   [`crate::protocol`]s.
//!
//! The `oidc` provider is deliberately not here: it needs an HTTP client and a
//! per-cloud exchange, and both belong beside the engine's existing STS code
//! rather than in this crate, which is otherwise dependency-light and entirely
//! testable without a network.
//!
//! # A helper runs unsandboxed, as you
//!
//! The helper argv is reviewed under CODEOWNERS, but the *binary* is resolved
//! from the host at mint time — unpinned, unhashed, outside any sandbox, with
//! heph's own environment. A `PATH`-hijacked `op` exfiltrates every credential
//! in the workspace. Two mitigations, and neither is a containment boundary:
//! prefer an absolute path, and name an [`Acquire::runner`] so the helper comes
//! from a described environment instead of an ambient lookup.

use crate::descriptor::{Acquire, Identity, Protocol, ProviderKind};
use crate::protocol;
use crate::redact::Redactor;
use crate::value::Credential;
use anyhow::Context as _;
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use hproc::proc_exec;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Seek, Write};
use std::sync::Arc;
use std::time::SystemTime;

/// A host environment lookup.
///
/// `Send + Sync` because a provider's future crosses a worker boundary, and a
/// closure is what makes this injectable — every test in this crate reads a
/// fixed table rather than the real process environment, which is what keeps
/// them parallel-safe.
pub type EnvLookup<'a> = &'a (dyn Fn(&str) -> Option<String> + Send + Sync);

/// Everything a provider is told. Nothing is discovered.
pub struct MintCtx<'a> {
    /// The descriptor target's address, for every diagnostic.
    pub addr: &'a str,
    /// Wall clock, injected so expiry logic is testable.
    pub now: SystemTime,
    /// Host environment lookup. The one ambient read this design permits, and
    /// only because nothing in the acquisition half reaches a cache key.
    pub env: EnvLookup<'a>,
    pub ctoken: &'a (dyn Cancellable + Send + Sync),
    /// Scopes runner resolution to the in-flight request.
    pub request_id: &'a str,
    /// Working directory for the helper.
    ///
    /// Handed in rather than read from the process. Reading `current_dir()`
    /// made `heph build //app:x` behave differently from the workspace root
    /// than from `app/`: a relative `helper` path, or any tool that walks up
    /// from the cwd looking for a `.envrc` or an `~/.aws/config`, could resolve
    /// to a different credential depending on where the build was invoked. The
    /// caller anchors this to the workspace root.
    pub cwd: &'a std::path::Path,
    /// The already-resolved [`Acquire::runner`], if the entry named one.
    ///
    /// Resolved by the caller rather than here, because turning an address into
    /// a built runner target needs the engine and this crate does not have it.
    pub runner: Option<&'a Addr>,
    /// Used to mask a failing helper's stderr before it reaches a diagnostic.
    /// A helper that fails *while printing the credential it just fetched* is
    /// not hypothetical.
    pub redactor: &'a Redactor,
}

/// The env closure and cancellation token are not `Debug`; the redactor is, and
/// prints counts rather than patterns.
impl std::fmt::Debug for MintCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintCtx")
            .field("addr", &self.addr)
            .field("request_id", &self.request_id)
            .field("runner", &self.runner.map(Addr::format))
            .field("cwd", &self.cwd)
            .field("redactor", self.redactor)
            .finish_non_exhaustive()
    }
}

/// A named way of obtaining a credential.
#[async_trait::async_trait]
pub trait SecretProvider: Send + Sync {
    /// The name an `acquire` entry selects this provider by.
    fn kind(&self) -> ProviderKind;

    async fn mint(
        &self,
        ctx: &MintCtx<'_>,
        identity: &Identity,
        acquire: &Acquire,
    ) -> anyhow::Result<Credential>;
}

/// Every provider this host knows.
///
/// Duplicate registration is a hard error, matching the runner registry: two
/// providers answering to one name means which one runs depends on
/// registration order, and that decides what identity a build ran as.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn SecretProvider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.providers.iter().map(|p| p.kind()))
            .finish()
    }
}

impl ProviderRegistry {
    /// The builtins, which is what a host wants unless it is a test.
    pub fn with_builtins() -> anyhow::Result<Self> {
        let mut r = Self::default();
        r.register(Arc::new(StaticEnvProvider))?;
        r.register(Arc::new(ExecProvider))?;
        Ok(r)
    }

    pub fn register(&mut self, p: Arc<dyn SecretProvider>) -> anyhow::Result<()> {
        if self.providers.iter().any(|e| e.kind() == p.kind()) {
            anyhow::bail!(
                "a secret provider for {:?} is already registered; two would make the identity a \
                 build ran as depend on registration order",
                p.kind()
            );
        }
        self.providers.push(p);
        Ok(())
    }

    pub fn get(&self, kind: ProviderKind) -> anyhow::Result<&Arc<dyn SecretProvider>> {
        self.providers
            .iter()
            .find(|p| p.kind() == kind)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no secret provider registered for {kind:?}. The `oidc` provider ships \
                     separately from the exec and static_env builtins."
                )
            })
    }
}

/// Read a value out of a named host environment variable.
///
/// The schema deliberately has no free-form value field, and this provider
/// names a variable rather than accepting a literal — otherwise someone writes
/// a token into a `text_file` target and it is pushed to the shared remote
/// cache.
#[derive(Debug)]
pub struct StaticEnvProvider;

#[async_trait::async_trait]
impl SecretProvider for StaticEnvProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::StaticEnv
    }

    async fn mint(
        &self,
        ctx: &MintCtx<'_>,
        _identity: &Identity,
        acquire: &Acquire,
    ) -> anyhow::Result<Credential> {
        let ttl = acquire.ttl_duration()?;
        let read = |var: &str| -> anyhow::Result<String> {
            match (ctx.env)(var) {
                Some(v) if !v.is_empty() => Ok(v),
                Some(_) => anyhow::bail!(
                    "secret {}: ${var} is set but empty. static_env treats that as unset, the \
                     same way an `acquire` guard does.",
                    ctx.addr
                ),
                None => anyhow::bail!(
                    "secret {}: ${var} is not set in this environment. static_env reads a host \
                     variable by name; export it, or move this descriptor to an `exec` or `oidc` \
                     acquisition.",
                    ctx.addr
                ),
            }
        };

        if let Some(var) = &acquire.var {
            let value = read(var)?;
            let expiry = crate::expiry::Expiry::resolve(ctx.now, None, Some(&value), ttl);
            return Ok(Credential::single(value, expiry));
        }

        let mut fields = BTreeMap::new();
        for (field, var) in &acquire.vars {
            fields.insert(field.clone(), crate::value::SecretValue::new(read(var)?));
        }
        if fields.is_empty() {
            anyhow::bail!("secret {}: static_env named no variables", ctx.addr);
        }
        Ok(Credential {
            fields,
            expiry: crate::expiry::Expiry::resolve(ctx.now, None, None, ttl),
        })
    }
}

/// Cancellation that fires on the build's token *or* on the helper deadline.
///
/// A deadline cannot simply drop the exec future: that abandons the child
/// rather than killing it, leaving an orphaned `op` or `docker-credential-*`
/// behind for every timed-out mint. `proc_exec::output` reaps on cancellation —
/// SIGKILL, then wait for the kernel to confirm — so the deadline has to arrive
/// *as* a cancellation rather than as a dropped future.
struct DeadlineToken<'a> {
    parent: &'a (dyn Cancellable + Send + Sync),
    deadline: hcore::hasync::StdCancellationToken,
}

impl Cancellable for DeadlineToken<'_> {
    fn is_cancelled(&self) -> bool {
        self.parent.is_cancelled() || self.deadline.is_cancelled()
    }

    fn cancelled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            tokio::select! {
                () = self.parent.cancelled() => {}
                () = self.deadline.cancelled() => {}
            }
        })
    }

    fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync> {
        // The parent outlives any detached use of this signal, and the deadline
        // half is already `'static`, so handing back the parent's own handle is
        // both correct and the only `'static` thing available.
        self.parent.clone_arc()
    }
}

/// Run a helper subprocess and read its output.
#[derive(Debug)]
pub struct ExecProvider;

impl ExecProvider {
    /// What a protocol needing a request URI is asking about.
    ///
    /// Drawn from the identity half, because it names the thing being
    /// authenticated to — which is exactly what an identity is.
    fn uri_for(protocol: Protocol, identity: &Identity) -> Option<String> {
        match protocol {
            Protocol::DockerCredential => identity.registry.clone(),
            Protocol::Engflow => identity
                .endpoint
                .clone()
                .or_else(|| identity.machine.clone().map(|m| format!("https://{m}"))),
            Protocol::CredentialProcess | Protocol::Raw => None,
        }
    }
}

#[async_trait::async_trait]
impl SecretProvider for ExecProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Exec
    }

    /// Every message built here is redacted **at construction**, so the
    /// `anyhow` chain survives intact.
    ///
    /// The earlier shape — run the whole thing, then flatten and redact the
    /// resulting chain — destroyed a downcast the engine depends on. Resolving
    /// a runner target goes back through the engine, so a Ctrl-C mid-mint
    /// arrives as a `CancelledError`, and `engine/result.rs` branches on
    /// `downcast_chain_ref` to tell a cancellation from a target failure. A
    /// flattened chain reads as an ordinary failure. Redacting each string as
    /// it is built costs nothing and keeps both properties, because every
    /// string that could carry a value is one this function writes.
    async fn mint(
        &self,
        ctx: &MintCtx<'_>,
        identity: &Identity,
        acquire: &Acquire,
    ) -> anyhow::Result<Credential> {
        let protocol = acquire.protocol.ok_or_else(|| {
            anyhow::anyhow!("secret {}: exec acquisition has no `protocol`", ctx.addr)
        })?;
        let (program, args) = acquire.helper.split_first().ok_or_else(|| {
            anyhow::anyhow!(
                "secret {}: exec acquisition has an empty `helper`",
                ctx.addr
            )
        })?;

        // The helper gets heph's own environment, because that is what it needs
        // to find a keychain, an SSO cache or a desktop-app session. Stated
        // rather than assumed: this is not a sandboxed process.
        let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();

        let stdin =
            match protocol::stdin_for(protocol, Self::uri_for(protocol, identity).as_deref()) {
                None => proc_exec::StdioSpec::Null,
                Some(payload) => {
                    // A file rather than a pipe: `proc_exec::output` has no stdin
                    // pump, and a payload this small does not justify hand-rolling
                    // the drain-while-writing dance that a pipe would need to avoid
                    // deadlocking on a helper that reads nothing.
                    //
                    // On the blocking pool, not the reactor: three syscalls is
                    // little, but a worker parked in `write` is a worker the
                    // runtime does not know is parked.
                    let f = hcore::blocking::run(move || -> std::io::Result<std::fs::File> {
                        let mut f = tempfile::tempfile()?;
                        f.write_all(&payload)?;
                        f.seek(std::io::SeekFrom::Start(0))?;
                        Ok(f)
                    })
                    .await
                    .with_context(|| format!("secret {}: helper stdin", ctx.addr))?;
                    proc_exec::StdioSpec::Fd(f.into())
                }
            };

        let spec = proc_exec::Spec {
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            env,
            cwd: ctx.cwd.to_path_buf(),
            stdin,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };

        let runner = match ctx.runner {
            None => hexecrunner::RunnerRef::local(),
            Some(addr) => hexecrunner::RunnerRef::target(ctx.request_id, addr),
        };

        // The deadline is what makes "never interactive during a build" true.
        // Closing stdin only stops a *stdin* prompt; a macOS keychain dialog, a
        // Touch ID prompt from `op`, or a helper blocked on an unreachable
        // endpoint reads no stdin at all and hangs a build that has nobody to
        // answer it. Under the broker's slot lock every consumer of that
        // descriptor queues behind it, so the build simply looks stuck.
        let deadline = acquire.helper_timeout()?;
        let token = DeadlineToken {
            parent: ctx.ctoken,
            deadline: hcore::hasync::StdCancellationToken::new(),
        };
        let mut timed_out = false;
        let run = hexecrunner::output(runner, spec, &token);
        tokio::pin!(run);
        let result = tokio::select! {
            r = &mut run => r,
            () = tokio::time::sleep(deadline) => {
                // Cancel, then await the same future: the exec path kills the
                // child and waits for the kernel to confirm the exit, so
                // nothing is left running behind a timed-out mint.
                timed_out = true;
                token.deadline.cancel();
                run.await
            }
        };
        if timed_out {
            anyhow::bail!(
                "{}",
                ctx.redactor.redact_str(&format!(
                    "secret {}: credential helper {:?} did not finish within {}.\n  It may be \
                     waiting on a desktop approval or a biometric prompt, which a build cannot \
                     answer. Run it once by hand to prime the session, or raise `timeout` on the \
                     acquire entry if it is legitimately slow.",
                    ctx.addr,
                    acquire.helper.join(" "),
                    humantime::format_duration(deadline),
                ))
            );
        }
        let out = result
            // `with_context`, not a stringify: resolving a runner target goes
            // back through the engine, so a Ctrl-C arrives as a `CancelledError`
            // the engine downcasts to tell cancellation from failure.
            .with_context(|| {
                ctx.redactor.redact_str(&format!(
                    "secret {}: running credential helper {:?}\n  A helper is resolved from the \
                     host at mint time. Prefer an absolute path, or name a `runner` on the \
                     acquire entry so it comes from a described environment.",
                    ctx.addr,
                    acquire.helper.join(" "),
                ))
            })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Last ten lines: helpers are chatty, and the useful sentence is
            // always at the end.
            let tail: String = {
                let mut lines: Vec<&str> = stderr.lines().rev().take(10).collect();
                lines.reverse();
                lines.join("\n")
            };
            anyhow::bail!(
                "{}",
                ctx.redactor.redact_str(&format!(
                    "secret {}: credential helper {:?} exited {}\n{}",
                    ctx.addr,
                    acquire.helper.join(" "),
                    out.status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "on a signal".to_string()),
                    if tail.is_empty() {
                        "  (it printed nothing to stderr)".to_string()
                    } else {
                        tail
                    }
                ))
            );
        }

        // A parse failure can quote what it was parsing, so it is redacted like
        // every other message built here.
        protocol::parse_response(protocol, &out.stdout, ctx.now, acquire.ttl_duration()?).map_err(
            |e| {
                anyhow::anyhow!(
                    "{}",
                    ctx.redactor
                        .redact_str(&format!("secret {}: {e:#}", ctx.addr))
                )
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::Protocol;
    use crate::expiry::ExpirySource;
    use hcore::hasync::StdCancellationToken;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    struct Ctx {
        token: StdCancellationToken,
        redactor: Redactor,
    }

    impl Ctx {
        fn new() -> Self {
            Self {
                token: StdCancellationToken::new(),
                redactor: Redactor::inert(),
            }
        }
        fn mint_ctx<'a>(&'a self, env: EnvLookup<'a>) -> MintCtx<'a> {
            MintCtx {
                addr: "//infra/creds:test",
                now: SystemTime::UNIX_EPOCH,
                env,
                ctoken: &self.token,
                request_id: "req",
                runner: None,
                cwd: std::path::Path::new("."),
                redactor: &self.redactor,
            }
        }
    }

    fn exec_acquire() -> Acquire {
        Acquire {
            helper: vec!["/bin/true".into()],
            protocol: Some(Protocol::Raw),
            ..acquire(ProviderKind::Exec)
        }
    }

    fn acquire(provider: ProviderKind) -> Acquire {
        Acquire {
            when_env: None,
            provider,
            var: None,
            vars: BTreeMap::new(),
            helper: Vec::new(),
            protocol: None,
            runner: None,
            exchange: None,
            timeout: None,
            ttl: None,
        }
    }

    #[tokio::test]
    async fn static_env_reads_a_named_variable() {
        let ctx = Ctx::new();
        let env = env_of(&[("GITHUB_TOKEN", "ghp_secret_value_here")]);
        let c = StaticEnvProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    var: Some("GITHUB_TOKEN".into()),
                    ..acquire(ProviderKind::StaticEnv)
                },
            )
            .await
            .expect("mint");
        assert_eq!(
            c.resolve_pointer("$.").expect("v").expose(),
            "ghp_secret_value_here"
        );
    }

    /// Set-but-empty is treated as unset here for the same reason it is in a
    /// `when_env` guard: CI systems blank a variable to mean "off".
    #[tokio::test]
    async fn static_env_treats_set_but_empty_as_unset() {
        let ctx = Ctx::new();
        let env = env_of(&[("TOK", "")]);
        let err = StaticEnvProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    var: Some("TOK".into()),
                    ..acquire(ProviderKind::StaticEnv)
                },
            )
            .await
            .expect_err("empty");
        assert!(err.to_string().contains("set but empty"), "{err}");
    }

    /// The failure has to say what to do, not just that a variable is missing.
    #[tokio::test]
    async fn a_missing_variable_names_itself_and_the_descriptor() {
        let ctx = Ctx::new();
        let env = env_of(&[]);
        let err = StaticEnvProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    var: Some("NOPE".into()),
                    ..acquire(ProviderKind::StaticEnv)
                },
            )
            .await
            .expect_err("unset");
        let msg = err.to_string();
        assert!(msg.contains("$NOPE"), "{msg}");
        assert!(msg.contains("//infra/creds:test"), "{msg}");
    }

    #[tokio::test]
    async fn static_env_multi_var_builds_the_aws_field_set() {
        let ctx = Ctx::new();
        let env = env_of(&[("AK", "AKIAEXAMPLE"), ("SK", "secretkeyvalue")]);
        let c = StaticEnvProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    vars: BTreeMap::from([
                        ("aws_access_key_id".to_string(), "AK".to_string()),
                        ("aws_secret_access_key".to_string(), "SK".to_string()),
                    ]),
                    ..acquire(ProviderKind::StaticEnv)
                },
            )
            .await
            .expect("mint");
        assert_eq!(
            c.get("aws_access_key_id").expect("k").expose(),
            "AKIAEXAMPLE"
        );
        assert_eq!(
            c.get("aws_secret_access_key").expect("k").expose(),
            "secretkeyvalue"
        );
    }

    #[tokio::test]
    async fn exec_raw_helper_reads_stdout() {
        let ctx = Ctx::new();
        let env = env_of(&[]);
        let c = ExecProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    helper: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "echo ghs_from_a_real_helper".into(),
                    ],
                    protocol: Some(Protocol::Raw),
                    ttl: Some("1h".into()),
                    ..acquire(ProviderKind::Exec)
                },
            )
            .await
            .expect("mint");
        assert_eq!(
            c.resolve_pointer("$.").expect("v").expose(),
            "ghs_from_a_real_helper"
        );
        assert_eq!(c.expiry.source, ExpirySource::DeclaredTtl);
    }

    /// The two protocols with a stdin payload must actually receive it — the
    /// docker one as a bare URL, which is the detail most often got wrong.
    #[tokio::test]
    async fn exec_writes_the_request_to_helper_stdin() {
        let ctx = Ctx::new();
        let env = env_of(&[]);
        let c = ExecProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity {
                    registry: Some("ghcr.io".into()),
                    ..Identity::default()
                },
                &Acquire {
                    // Echo back what arrived on stdin as the Secret, so the
                    // assertion proves the payload crossed.
                    helper: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        r#"printf '{"Username":"<token>","Secret":"%s-ok"}' "$(cat)""#.into(),
                    ],
                    protocol: Some(Protocol::DockerCredential),
                    ..acquire(ProviderKind::Exec)
                },
            )
            .await
            .expect("mint");
        assert_eq!(c.resolve_pointer("$.").expect("v").expose(), "ghcr.io-ok");
        assert_eq!(c.get("Username").expect("u").expose(), "<token>");
    }

    /// A non-zero exit aborts the mint and surfaces the helper's own stderr,
    /// because that is the only thing that explains why.
    #[tokio::test]
    async fn a_failing_helper_reports_its_stderr() {
        let ctx = Ctx::new();
        let env = env_of(&[]);
        let err = ExecProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    helper: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "echo 'not logged in: run gh auth login' >&2; exit 4".into(),
                    ],
                    protocol: Some(Protocol::Raw),
                    ..acquire(ProviderKind::Exec)
                },
            )
            .await
            .expect_err("exit 4");
        let msg = err.to_string();
        assert!(msg.contains("exited 4"), "{msg}");
        assert!(msg.contains("run gh auth login"), "{msg}");
    }

    /// A helper that fails while echoing the credential it just fetched is not
    /// hypothetical, and its stderr goes into a build log.
    #[tokio::test]
    async fn a_failing_helpers_stderr_is_redacted() {
        let (redactor, _) = Redactor::new(&[crate::redact::Entry {
            name: "gh",
            value: "ghs_16C7e42F292c6912E7710c838347Ae178B4a",
        }]);
        let token = StdCancellationToken::new();
        let env = env_of(&[]);
        let ctx = MintCtx {
            addr: "//infra/creds:test",
            now: SystemTime::UNIX_EPOCH,
            env: &env,
            ctoken: &token,
            request_id: "req",
            runner: None,
            cwd: std::path::Path::new("."),
            redactor: &redactor,
        };
        let err = ExecProvider
            .mint(
                &ctx,
                &Identity::default(),
                &Acquire {
                    helper: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "echo 'failed with ghs_16C7e42F292c6912E7710c838347Ae178B4a' >&2; exit 1"
                            .into(),
                    ],
                    protocol: Some(Protocol::Raw),
                    ..acquire(ProviderKind::Exec)
                },
            )
            .await
            .expect_err("exit 1");
        let msg = err.to_string();
        assert!(
            !msg.contains("ghs_16C7"),
            "credential leaked into a diagnostic: {msg}"
        );
        assert!(msg.contains("«redacted:gh»"), "{msg}");
    }

    /// A helper that hangs must fail with a message naming it, not hang the
    /// build. Closing stdin does not cover a keychain dialog or a blocked
    /// network call, which is why the deadline exists.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_helper_hits_its_deadline_and_says_what_to_do() {
        let ctx = Ctx::new();
        let env = env_of(&[]);
        let err = ExecProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    // Reads no stdin and never exits — the shape of a helper
                    // waiting on a desktop approval.
                    helper: vec!["/bin/sh".into(), "-c".into(), "sleep 3600".into()],
                    protocol: Some(Protocol::Raw),
                    timeout: Some("2s".into()),
                    ..acquire(ProviderKind::Exec)
                },
            )
            .await
            .expect_err("deadline");
        let msg = format!("{err:#}");
        assert!(msg.contains("did not finish within 2s"), "{msg}");
        assert!(msg.contains("/bin/sh"), "{msg}");
        assert!(msg.contains("desktop approval"), "{msg}");
    }

    #[test]
    fn the_helper_deadline_defaults_and_is_overridable() {
        let default = exec_acquire();
        assert_eq!(
            default.helper_timeout().expect("default"),
            crate::descriptor::DEFAULT_HELPER_TIMEOUT
        );

        let slow = Acquire {
            timeout: Some("5m".into()),
            ..exec_acquire()
        };
        assert_eq!(
            slow.helper_timeout().expect("override"),
            std::time::Duration::from_secs(300)
        );

        // A deadline on a provider that runs no subprocess is a mistake worth
        // naming rather than silently ignoring.
        let wrong = Acquire {
            timeout: Some("5m".into()),
            var: Some("TOK".into()),
            ..acquire(ProviderKind::StaticEnv)
        };
        let err = wrong.validate("//x:y", 0).expect_err("not an exec");
        assert!(err.to_string().contains("only applies to an exec"), "{err}");
    }

    /// The advice a reader needs is "prefer an absolute path or a runner", not
    /// a bare ENOENT.
    #[tokio::test]
    async fn a_missing_helper_binary_says_what_to_do() {
        let ctx = Ctx::new();
        let env = env_of(&[]);
        let err = ExecProvider
            .mint(
                &ctx.mint_ctx(&env),
                &Identity::default(),
                &Acquire {
                    helper: vec!["/nonexistent/definitely-not-here".into()],
                    protocol: Some(Protocol::Raw),
                    ..acquire(ProviderKind::Exec)
                },
            )
            .await
            .expect_err("no such binary");
        let msg = format!("{err:#}");
        assert!(msg.contains("absolute path"), "{msg}");
        assert!(msg.contains("`runner`"), "{msg}");
    }

    #[test]
    fn duplicate_provider_registration_is_refused() {
        let mut r = ProviderRegistry::with_builtins().expect("builtins");
        let err = r.register(Arc::new(ExecProvider)).expect_err("duplicate");
        assert!(err.to_string().contains("already registered"), "{err}");
    }

    #[test]
    fn an_unregistered_provider_says_which_one_is_missing() {
        let r = ProviderRegistry::with_builtins().expect("builtins");
        assert!(r.get(ProviderKind::Exec).is_ok(), "exec is a builtin");
        let err = r
            .get(ProviderKind::Oidc)
            .map(|_| ())
            .expect_err("oidc ships separately");
        assert!(err.to_string().contains("Oidc"), "{err}");
    }

    #[test]
    fn only_the_two_request_protocols_derive_a_uri() {
        let id = Identity {
            registry: Some("ghcr.io".into()),
            machine: Some("github.com".into()),
            ..Identity::default()
        };
        assert_eq!(
            ExecProvider::uri_for(Protocol::DockerCredential, &id).as_deref(),
            Some("ghcr.io")
        );
        assert_eq!(
            ExecProvider::uri_for(Protocol::Engflow, &id).as_deref(),
            Some("https://github.com")
        );
        assert!(ExecProvider::uri_for(Protocol::Raw, &id).is_none());
        assert!(ExecProvider::uri_for(Protocol::CredentialProcess, &id).is_none());
    }
}
