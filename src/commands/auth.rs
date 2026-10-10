//! `heph auth` — the preflight, and the one place a human signs in.
//!
//! # Why this is top-level and not `heph tool auth`
//!
//! It is a command a developer runs directly and often, which is the line the
//! `tool` group is on the other side of, and it matches the shape of every vendor
//! CLI a user already has muscle memory for. That also makes it frozen once
//! shipped: moving it later would need a permanent alias.
//!
//! # A build never signs anyone in
//!
//! It fails with the exact command to run, and a human runs it here, in the
//! foreground, where heph owns the terminal because it is the only thing using
//! it. That removes a whole seam — nothing has to hand a child process the
//! terminal mid-build — and it removes an event: with no prompt during a build,
//! no login URL can reach the hook stream and end up in a pull-request comment.
//!
//! # And even `login` does not require a tty
//!
//! An agent's stderr is not a terminal. It runs the command, relays the URL and
//! code a human needs, and the human finishes elsewhere. Demanding a terminal
//! would strand the agent with no path forward.

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::progress_app::{request_state, run_with_progress};
use crate::engine::credential::source_login;
use crate::engine::{Engine, get_cwp};
use crate::tui::{self, AppContext, LogSink};
use anyhow::Context as _;
use clap::{Args, Subcommand};
use hbuiltins::plugincredential::{CredentialDef, DRIVER_NAME, parse_declaration};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use std::sync::Arc;

#[derive(Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommands,
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// One row per declared credential: is it available here, from where, and
    /// until when
    ///
    /// The preflight. Exits non-zero unless every row is `ok`, so a caller can
    /// branch without parsing and then parse to learn what to run.
    ///
    /// Probes each chain; it does not acquire anything, so it runs no vendor CLI
    /// and signs nobody in.
    ///
    /// Examples:
    ///
    /// `heph auth status`
    ///
    /// `heph auth status //auth/... --json`
    Status(StatusArgs),
    /// The whole chain walk: every source, why each was skipped, which won
    ///
    /// What to run when `status` says something is unavailable and the reason is
    /// not obvious. Prints the same structure the "no source applies" build
    /// failure does, so the diagnostic and the failure can never drift apart.
    ///
    /// Example: `heph auth explain //auth:aws`
    Explain(ExplainArgs),
    /// Run whichever sign-in commands the probe found stale
    ///
    /// Never requires a terminal: it runs the vendor's own command with this
    /// process's stdio attached, so a browser flow works interactively and an
    /// agent sees the URL and code on its own output.
    ///
    /// Examples:
    ///
    /// `heph auth login` — everything that has gone stale
    ///
    /// `heph auth login //auth:aws` — one credential
    Login(LoginArgs),
    /// Forget every acquired credential, in this process and on disk
    ///
    /// Clears `<home>/auth`. The vendor CLIs' own sessions are untouched — heph
    /// does not own those, and signing you out of `aws` because you asked heph to
    /// forget a cache entry would be a surprise.
    Logout,
}

#[derive(Args, Clone)]
pub struct StatusArgs {
    /// Package matcher limiting which credentials are listed. Defaults to the
    /// whole workspace.
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub matcher: Option<String>,
    /// Machine-readable output: `addr`, `state`, `source`, `expires_at`, `login`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone)]
pub struct ExplainArgs {
    /// The credential to explain.
    #[arg(value_name = "TARGET_ADDRESS")]
    pub addr: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone)]
pub struct LoginArgs {
    /// Credential to sign in to. Defaults to every one that has gone stale.
    #[arg(value_name = "TARGET_ADDRESS")]
    pub addr: Option<String>,
}

impl AuthArgs {
    /// `status`, `explain` and `login` can build targets — a source's `runner`
    /// is one, and probing under it builds it — so they run under the TUI like
    /// `run` does, rather than sitting silent for as long as a devenv shell
    /// takes to build. `logout` touches only the credential store.
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        match &self.command {
            AuthCommands::Status(a) => {
                bootstrap::block_on(status(a.clone(), sink, global.clone()))?
            }
            AuthCommands::Explain(a) => {
                bootstrap::block_on(explain(a.clone(), sink, global.clone()))?
            }
            AuthCommands::Login(a) => bootstrap::block_on(login(a.clone(), sink, global.clone()))?,
            AuthCommands::Logout => bootstrap::block_on(logout())?,
        }
    }
}

/// What `status` says about one credential.
///
/// Three states, and the distinction between the last two is the whole value of
/// the command: "you need to run something" is actionable, "nothing here can
/// work" is a configuration bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    /// A source applies here.
    Ok,
    /// No source applies, but one of them has a sign-in command.
    NeedsSignIn,
    /// No source applies and none can be repaired by signing in.
    Unavailable,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NeedsSignIn => "needs sign-in",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(serde::Serialize)]
struct Row {
    addr: String,
    state: State,
    /// The winning source, or the first one that could be repaired.
    source: String,
    /// Absolute, never a relative `expires_in` — which is only true at the moment
    /// it was emitted, and a caller that stores the answer would be wrong by
    /// however long it took to get there.
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    /// The sign-in commands a human should run, as structured data rather than
    /// prose, so `--json` consumers and the terminal render the same instruction.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    login: Vec<Vec<String>>,
}

async fn status(args: StatusArgs, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let m = credential_matcher(args.matcher.as_deref())?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let label = format!(
        "Checking credentials in {}",
        args.matcher.as_deref().unwrap_or("//...")
    );
    run_with_progress(label, sink, global.no_tui, shutdown, move |ctx| {
        status_body(engine, args, m, global.fail_fast, ctx)
    })
    .await
}

async fn status_body(
    engine: Arc<Engine>,
    args: StatusArgs,
    m: Matcher,
    fail_fast: bool,
    ctx: AppContext,
) -> anyhow::Result<()> {
    let rs = request_state(&engine, &ctx);
    let gaps = crate::engine::Gaps::new(args.matcher.as_deref().unwrap_or("//..."));
    let discovery = crate::engine::Discovery::keep_going_unless(fail_fast, &gaps);
    let creds = credentials_matching(&engine, &rs, &m, discovery).await?;

    let mut rows = Vec::with_capacity(creds.len());
    for (addr, def) in &creds {
        rows.push(row_for(&engine, &rs, addr, def).await);
    }

    // Rendered whole, then printed with the TUI paused: the table must not land
    // inside a live frame.
    let out = render_status(&rows, gaps.is_empty(), args.json)?;
    if !out.is_empty() {
        tui::paused!(ctx, { print!("{out}") });
    }

    // A credential the walk could not reach is not in the table at all, which
    // is worse than one that is not ok.
    crate::commands::errors::require_complete_selection(&gaps)?;
    status_verdict(&rows)
}

/// What `status` prints on stdout. `complete` is whether the walk reached the
/// whole selection.
fn render_status(rows: &[Row], complete: bool, json: bool) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    if json {
        let mut s = serde_json::to_string_pretty(rows).context("render --json")?;
        s.push('\n');
        return Ok(s);
    }
    if rows.is_empty() {
        // "None here" would be a claim the walk cannot make if it skipped
        // part of the workspace; the incomplete block says that instead.
        return Ok(if complete {
            "no `credential` targets in this workspace\n".to_string()
        } else {
            String::new()
        });
    }
    let mut out = String::new();
    let widest = rows.iter().map(|r| r.addr.len()).max().unwrap_or(0);
    for r in rows {
        let tail = match (&r.expires_at, r.login.first()) {
            (Some(at), _) => format!("expires {at}"),
            (None, Some(_)) => format!("run: heph auth login {}", r.addr),
            (None, None) => String::new(),
        };
        writeln!(
            out,
            "{:<widest$}  {:<14} {:<24} {tail}",
            r.addr,
            r.state.label(),
            r.source
        )
        .context("render the status table")?;
    }
    Ok(out)
}

/// The exit of `status`: non-zero unless every row is ok, so a caller can
/// branch on the exit status and only then parse to learn what to run.
fn status_verdict(rows: &[Row]) -> anyhow::Result<()> {
    if rows.iter().any(|r| r.state != State::Ok) {
        anyhow::bail!(
            "{} of {} credentials are not available here — `heph auth explain <addr>` says why",
            rows.iter().filter(|r| r.state != State::Ok).count(),
            rows.len()
        );
    }
    Ok(())
}

async fn row_for(
    engine: &Arc<Engine>,
    rs: &Arc<crate::engine::request_state::RequestState>,
    addr: &Addr,
    def: &CredentialDef,
) -> Row {
    let walk = engine.walk_chain(rs, addr, def).await;
    let chosen = walk.iter().find(|s| s.skipped.is_none());
    match chosen {
        Some(step) => Row {
            addr: addr.format(),
            state: State::Ok,
            source: step.label.clone(),
            // Best-effort: the disk tier is keyed on the chosen source *and the
            // identity that acquired it*, and status deliberately acquires
            // nothing — so a credential whose source needs its own credentials
            // reports no expiry rather than a guessed one.
            expires_at: def
                .sources
                .get(step.index)
                .and_then(|s| cached_expiry(engine, addr, s)),
            login: Vec::new(),
        },
        None => {
            let repairable = walk
                .iter()
                .filter_map(|s| def.sources.get(s.index).map(|src| (s, src)))
                .find(|(_, src)| !source_login(src).is_empty());
            match repairable {
                Some((step, src)) => Row {
                    addr: addr.format(),
                    state: State::NeedsSignIn,
                    source: step.label.clone(),
                    expires_at: None,
                    login: source_login(src).to_vec(),
                },
                None => Row {
                    addr: addr.format(),
                    state: State::Unavailable,
                    source: walk
                        .first()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| "—".to_string()),
                    expires_at: None,
                    login: Vec::new(),
                },
            }
        }
    }
}

/// The expiry of a cached entry for this credential + source, when one exists.
fn cached_expiry(
    engine: &Arc<Engine>,
    addr: &Addr,
    source: &hbuiltins::plugincredential::SourceDecl,
) -> Option<String> {
    let store = crate::engine::CredentialStore::new(&engine.home);
    let key = crate::engine::credential_store::resolution_key(
        &addr.format(),
        &serde_json::to_string(&source.kind).unwrap_or_default(),
        &[],
    );
    let secs = store
        .get(&key, std::time::SystemTime::now())?
        .material
        .expires_at?;
    let at = chrono::DateTime::from_timestamp(secs as i64, 0)?;
    Some(at.to_rfc3339())
}

#[derive(serde::Serialize)]
struct ExplainStep {
    index: usize,
    source: String,
    chosen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<String>,
    hint: String,
}

async fn explain(args: ExplainArgs, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let cwp = get_cwp()?;
    let addr = hmodel::htaddr::parse_addr_with_base(&args.addr, &cwp)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let label = format!("Explaining {addr}");
    run_with_progress(label, sink, global.no_tui, shutdown, move |ctx| {
        explain_body(engine, addr, args.json, ctx)
    })
    .await
}

async fn explain_body(
    engine: Arc<Engine>,
    addr: Addr,
    json: bool,
    ctx: AppContext,
) -> anyhow::Result<()> {
    let rs = request_state(&engine, &ctx);
    let spec = Arc::clone(&engine).get_spec(rs.clone(), &addr).await?;
    if spec.driver != DRIVER_NAME {
        anyhow::bail!(
            "{addr} is a `{}` target, not a credential — `heph auth explain` walks a credential's \
             source chain",
            spec.driver
        );
    }
    let def = parse_declaration(&spec)?;
    let walk = engine.walk_chain(&rs, &addr, &def).await;
    let steps: Vec<ExplainStep> = walk
        .iter()
        .map(|s| ExplainStep {
            index: s.index,
            source: s.label.clone(),
            chosen: s.skipped.is_none(),
            skipped: s.skipped.clone(),
            hint: s.hint.clone(),
        })
        .collect();

    let out = render_explain(&addr, &steps, json)?;
    tui::paused!(ctx, { print!("{out}") });
    // `--json` reports the walk and exits 0 either way: whether a source was
    // chosen is in the document.
    if !json && !steps.iter().any(|s| s.chosen) {
        anyhow::bail!("no source applies here");
    }
    Ok(())
}

/// What `explain` prints on stdout.
fn render_explain(addr: &Addr, steps: &[ExplainStep], json: bool) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    if json {
        let mut s = serde_json::to_string_pretty(&serde_json::json!({
            "addr": addr.format(),
            "sources": steps,
        }))
        .context("render --json")?;
        s.push('\n');
        return Ok(s);
    }
    let mut out = String::new();
    let mut line = |s: std::fmt::Arguments<'_>| {
        out.write_fmt(s)
            .and_then(|()| out.write_char('\n'))
            .context("render the source chain")
    };
    line(format_args!("{addr}"))?;
    for s in steps {
        match &s.skipped {
            None => line(format_args!(
                "  {}. {:<24} applies here",
                s.index + 1,
                s.source
            ))?,
            Some(reason) => {
                line(format_args!(
                    "  {}. {:<24} skipped: {reason}",
                    s.index + 1,
                    s.source
                ))?;
                line(format_args!("{:28}→ {}", "", s.hint))?;
            }
        }
    }
    Ok(out)
}

async fn login(args: LoginArgs, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, shutdown) = bootstrap::new_engine()?;
    let label = match &args.addr {
        Some(a) => format!("Signing in to {a}"),
        None => "Signing in".to_string(),
    };
    let wanted = match &args.addr {
        Some(raw) => Wanted::One(hmodel::htaddr::parse_addr_with_base(raw, &get_cwp()?)?),
        None => Wanted::All(credential_matcher(None)?),
    };
    run_with_progress(label, sink, global.no_tui, shutdown, move |ctx| {
        login_body(engine, wanted, global.fail_fast, ctx)
    })
    .await
}

/// What `login` signs in to.
enum Wanted {
    /// The one credential named on the command line.
    One(Addr),
    /// Every credential under the matcher that has gone stale.
    All(Matcher),
}

async fn login_body(
    engine: Arc<Engine>,
    wanted: Wanted,
    fail_fast: bool,
    ctx: AppContext,
) -> anyhow::Result<()> {
    let rs = request_state(&engine, &ctx);
    let gaps = crate::engine::Gaps::new("//...");
    let creds = match wanted {
        Wanted::One(addr) => {
            let spec = Arc::clone(&engine).get_spec(rs.clone(), &addr).await?;
            if spec.driver != DRIVER_NAME {
                anyhow::bail!("{addr} is a `{}` target, not a credential", spec.driver);
            }
            vec![(addr, parse_declaration(&spec)?)]
        }
        Wanted::All(m) => {
            let discovery = crate::engine::Discovery::keep_going_unless(fail_fast, &gaps);
            credentials_matching(&engine, &rs, &m, discovery).await?
        }
    };

    let pauser = ctx.pauser();
    let mut ran = 0usize;
    for (addr, def) in &creds {
        let walk = engine.walk_chain(&rs, addr, def).await;
        // Only sources the probe found *stale*. Running a login for a source
        // that already works would re-open a browser for nothing — and gcloud's
        // two independent logins are exactly why this is per-source rather than
        // per-credential.
        for step in walk.iter().filter(|s| s.skipped.is_some()) {
            let Some(src) = def.sources.get(step.index) else {
                continue;
            };
            for argv in source_login(src) {
                if argv.is_empty() {
                    continue;
                }
                // The vendor command gets the terminal for as long as it runs:
                // the TUI steps aside — cooked mode, and its key reader torn
                // down so it cannot eat the keystrokes a device-code prompt
                // reads — and comes back when the command exits. `Input`, not
                // `Output`: the user is typing at the vendor CLI, so their Ctrl-C
                // is for it; it dies of the SIGINT and the failure below ends the
                // command. A no-op on the line backend.
                let status = {
                    let _terminal = pauser.pause_for(tui::PauseFor::Input).await;
                    println!("{addr}: {}", argv.join(" "));
                    // Through the engine's exec-runner seam, so a source whose
                    // tool lives in a devenv shell signs in *there* — the same
                    // place its probe looked. Stdio is inherited: a browser flow
                    // needs the terminal when there is one, and an agent needs
                    // to see the URL and code on its own output when there is
                    // not.
                    engine.run_login(&rs, addr, src, argv).await?
                };
                if !status.success() {
                    anyhow::bail!("{addr}: `{}` exited with {status}", argv.join(" "));
                }
                ran += 1;
            }
        }
    }
    if ran == 0 && gaps.is_empty() {
        tui::paused!(ctx, {
            println!("nothing to sign in to — every declared credential already applies here")
        });
    }
    crate::commands::errors::require_complete_selection(&gaps)
}

async fn logout() -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    // Both tiers. The process one is empty in a fresh `heph auth logout`, but
    // clearing only the disk would make this command mean something different
    // depending on who called it.
    engine.forget_credentials();
    let store = crate::engine::CredentialStore::new(&engine.home);
    store.clear()?;
    println!("cleared {}", store.root().display());
    Ok(())
}

/// The selection `credentials_matching` walks: the workspace, or `matcher`.
fn credential_matcher(matcher: Option<&str>) -> anyhow::Result<Matcher> {
    let cwp = get_cwp()?;
    // Through the expression slot, not the positional one: the single-positional
    // form takes an *address*, and the selection here is a package matcher.
    crate::commands::utils::resolve_matcher(
        &Some(matcher.unwrap_or("//...").to_string()),
        &None,
        &None,
        &cwp,
        true,
    )
}

/// Every `credential` target under `m`.
///
/// Selects with `driver(auth.credential)`, so a provider that lists each target's
/// driver (go, buildfile) decides every other target from its listing without
/// resolving it — no `go list` anywhere. Only a candidate whose driver is
/// unknown is resolved to decide.
///
/// Unless `--fail-fast`, keeps going past a candidate that cannot be resolved:
/// a broken package elsewhere in the workspace must not hide the credentials
/// that do resolve. The caller reports the skips once it has shown what it
/// found.
async fn credentials_matching(
    engine: &Arc<Engine>,
    rs: &Arc<crate::engine::request_state::RequestState>,
    m: &Matcher,
    discovery: crate::engine::Discovery,
) -> anyhow::Result<Vec<(Addr, CredentialDef)>> {
    use futures::TryStreamExt as _;
    let m = Matcher::And(vec![m.clone(), Matcher::Driver(DRIVER_NAME.to_string())]);
    // `query_spec`, not `query` + `get_spec`: a listed candidate may not resolve
    // standalone (go's per-platform variants), and that is not a broken target.
    let stream = Arc::clone(engine).query_spec(rs.clone(), &m, discovery);
    tokio::pin!(stream);
    let mut out = Vec::new();
    while let Some(spec) = stream.try_next().await? {
        // Defensive: a listed Yes is confirmed to exist, not to be a credential.
        if spec.driver != DRIVER_NAME {
            continue;
        }
        let addr = spec.addr.clone();
        let def = parse_declaration(&spec).with_context(|| format!("credential {addr}"))?;
        out.push((addr, def));
    }
    out.sort_by_key(|a| a.0.format());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::provider::{
        ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
        ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider,
    };
    use futures::future::BoxFuture;
    use hbuiltins::pluginstatictarget;
    use hcore::hasync::Cancellable;
    use hcore::htvalue::Value;
    use hmodel::htpkg::PkgBuf;

    /// Lists `//auth:phantom` and can `get` nothing — the shape of go's
    /// per-platform variants, which `list` advertises and `get` declines.
    struct PhantomLister;

    impl Provider for PhantomLister {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "phantom".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            let addr = Addr::new(
                req.package.clone(),
                "phantom".to_string(),
                Default::default(),
            );
            Box::pin(async move {
                let items = vec![Ok(ListResponse::addr_only(addr))];
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            Box::pin(async { Err(GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    /// `heph auth login` walks the whole workspace for credentials, so one
    /// listed-but-unresolvable candidate anywhere used to fail it with
    /// `target not found`.
    #[tokio::test]
    async fn an_unresolvable_candidate_does_not_fail_the_credential_walk() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: root.path().join(".heph3"),
            parallelism: None,
            ..Default::default()
        })?;
        let s = |v: &str| Value::String(v.to_string());
        let credential = pluginstatictarget::Target {
            addr: "//auth:token".to_string(),
            driver: DRIVER_NAME.to_string(),
            raw_config: [
                (
                    "sources".to_string(),
                    Value::List(vec![Value::Map(
                        [
                            ("kind".to_string(), s("env")),
                            ("names".to_string(), Value::List(vec![s("MY_TOKEN")])),
                        ]
                        .into(),
                    )]),
                ),
                (
                    "present".to_string(),
                    Value::Map(
                        [(
                            "env".to_string(),
                            Value::Map([("MY_TOKEN".to_string(), s("${my_token}"))].into()),
                        )]
                        .into(),
                    ),
                ),
            ]
            .into(),
            ..Default::default()
        };
        let provider = pluginstatictarget::Provider::new(vec![credential])?;
        engine.register_provider(move |_| Box::new(provider))?;
        engine.register_provider(|_| Box::new(PhantomLister))?;
        let engine = Arc::new(engine);

        let rs = engine.new_state();
        let gaps = crate::engine::Gaps::new("//...");
        let creds = credentials_matching(
            &engine,
            &rs,
            &Matcher::PackagePrefix(PkgBuf::from("")),
            crate::engine::Discovery::KeepGoing(Arc::clone(&gaps)),
        )
        .await?;

        let addrs: Vec<String> = creds.iter().map(|(a, _)| a.format()).collect();
        assert_eq!(addrs, vec!["//auth:token".to_string()]);
        assert!(
            gaps.is_empty(),
            "a candidate that was never there is not an incomplete selection"
        );
        Ok(())
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn env_source(name: &str) -> Value {
        Value::Map(
            [
                ("kind".to_string(), s("env")),
                ("names".to_string(), Value::List(vec![s(name)])),
            ]
            .into(),
        )
    }

    /// An `exec` source whose tool is nowhere on PATH, so the probe finds it
    /// stale and `login` runs `login_argv`.
    fn stale_exec_source(login_argv: &str) -> Value {
        Value::Map(
            [
                ("kind".to_string(), s("exec")),
                (
                    "run".to_string(),
                    Value::List(vec![s("heph-auth-test-no-such-tool")]),
                ),
                ("login".to_string(), Value::List(vec![s(login_argv)])),
            ]
            .into(),
        )
    }

    /// An engine whose only target is `//auth:token`, a credential with
    /// `sources`.
    fn credential_engine(
        root: &std::path::Path,
        sources: Vec<Value>,
    ) -> anyhow::Result<Arc<Engine>> {
        let mut engine = Engine::new(Config {
            root: root.to_path_buf(),
            home_dir: root.join(".heph3"),
            parallelism: None,
            ..Default::default()
        })?;
        let credential = pluginstatictarget::Target {
            addr: "//auth:token".to_string(),
            driver: DRIVER_NAME.to_string(),
            raw_config: [
                ("sources".to_string(), Value::List(sources)),
                (
                    "present".to_string(),
                    Value::Map(
                        [(
                            "env".to_string(),
                            Value::Map([("TOKEN".to_string(), s("${token}"))].into()),
                        )]
                        .into(),
                    ),
                ),
            ]
            .into(),
            ..Default::default()
        };
        let provider = pluginstatictarget::Provider::new(vec![credential])?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    fn token() -> Addr {
        hmodel::htaddr::parse_addr("//auth:token").expect("addr")
    }

    /// The status body under the line backend an agent gets — the path the
    /// command takes with no tty.
    async fn run_status(engine: Arc<Engine>, json: bool) -> anyhow::Result<()> {
        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        let args = StatusArgs {
            matcher: None,
            json,
        };
        run_with_progress("t", LogSink::new_direct(), true, shutdown, move |ctx| {
            status_body(
                engine,
                args,
                Matcher::PackagePrefix(PkgBuf::from("")),
                false,
                ctx,
            )
        })
        .await
    }

    /// Moving `status` under the progress app must not move its exit: zero when
    /// every row is ok, non-zero — with the count — when one is not, `--json`
    /// or not.
    #[tokio::test]
    async fn status_exits_non_zero_unless_every_row_is_ok() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        // PATH is set wherever a test runs.
        let ok = credential_engine(root.path(), vec![env_source("PATH")])?;
        run_status(Arc::clone(&ok), false).await?;
        run_status(ok, true).await?;

        let root = tempfile::tempdir()?;
        let unset = || env_source("HEPH_AUTH_TEST_CERTAINLY_UNSET_TOKEN");
        for json in [false, true] {
            let missing = credential_engine(root.path(), vec![unset()])?;
            let err = run_status(missing, json)
                .await
                .expect_err("an unavailable credential fails the preflight");
            assert!(
                err.to_string().contains("1 of 1 credentials"),
                "json={json}: {err:#}"
            );
        }
        Ok(())
    }

    async fn run_explain(engine: Arc<Engine>, json: bool) -> anyhow::Result<()> {
        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        run_with_progress("t", LogSink::new_direct(), true, shutdown, move |ctx| {
            explain_body(engine, token(), json, ctx)
        })
        .await
    }

    /// `explain` fails when nothing applies — except under `--json`, where
    /// whether a source was chosen is in the document.
    #[tokio::test]
    async fn explain_fails_when_no_source_applies_unless_json() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let ok = credential_engine(root.path(), vec![env_source("PATH")])?;
        run_explain(ok, false).await?;

        let root = tempfile::tempdir()?;
        let unset = || env_source("HEPH_AUTH_TEST_CERTAINLY_UNSET_TOKEN");
        let err = run_explain(credential_engine(root.path(), vec![unset()])?, false)
            .await
            .expect_err("nothing applies");
        assert!(err.to_string().contains("no source applies"), "{err:#}");
        run_explain(credential_engine(root.path(), vec![unset()])?, true).await?;
        Ok(())
    }

    async fn run_login(engine: Arc<Engine>) -> anyhow::Result<()> {
        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        run_with_progress("t", LogSink::new_direct(), true, shutdown, move |ctx| {
            login_body(engine, Wanted::One(token()), false, ctx)
        })
        .await
    }

    /// `login` runs a stale source's sign-in command with the terminal handed
    /// over (a no-op pause on the line backend) and takes its exit: a failed
    /// sign-in fails the command.
    #[tokio::test]
    async fn login_runs_the_stale_sign_in_and_takes_its_exit() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        run_login(credential_engine(
            root.path(),
            vec![stale_exec_source("true")],
        )?)
        .await?;

        let root = tempfile::tempdir()?;
        let err = run_login(credential_engine(
            root.path(),
            vec![stale_exec_source("false")],
        )?)
        .await
        .expect_err("a failed sign-in fails the command");
        assert!(err.to_string().contains("`false` exited with"), "{err:#}");

        // Nothing stale: nothing runs, and that is a success.
        let root = tempfile::tempdir()?;
        run_login(credential_engine(root.path(), vec![env_source("PATH")])?).await?;
        Ok(())
    }

    fn row(addr: &str, state: State, source: &str) -> Row {
        Row {
            addr: addr.to_string(),
            state,
            source: source.to_string(),
            expires_at: None,
            login: vec![],
        }
    }

    /// The table is what it was before it moved behind the TUI, byte for byte.
    #[test]
    fn the_status_table_renders_unchanged() -> anyhow::Result<()> {
        let mut aws = row("//auth:aws", State::NeedsSignIn, "exec(aws)");
        aws.login = vec![vec![
            "aws".to_string(),
            "sso".to_string(),
            "login".to_string(),
        ]];
        let mut gh = row("//auth:gh", State::Ok, "env(GH_TOKEN)");
        gh.expires_at = Some("2026-09-08T15:41:00+00:00".to_string());
        let ci = row("//ci:x", State::Unavailable, "oidc(github_actions)");
        let rows = [aws, gh, ci];

        assert_eq!(
            render_status(&rows, true, false)?,
            "//auth:aws  needs sign-in  exec(aws)                run: heph auth login //auth:aws\n\
             //auth:gh   ok             env(GH_TOKEN)            expires 2026-09-08T15:41:00+00:00\n\
             //ci:x      unavailable    oidc(github_actions)     \n"
        );

        let json: serde_json::Value = serde_json::from_str(&render_status(&rows, true, true)?)?;
        assert_eq!(json.as_array().map(Vec::len), Some(3));
        assert_eq!(json[0]["state"], "needs_sign_in");
        Ok(())
    }

    /// An empty table says "none here" only when the walk reached everything;
    /// otherwise the incomplete-selection block is the answer.
    #[test]
    fn an_empty_status_claims_none_only_for_a_complete_walk() -> anyhow::Result<()> {
        assert_eq!(
            render_status(&[], true, false)?,
            "no `credential` targets in this workspace\n"
        );
        assert_eq!(render_status(&[], false, false)?, "");
        assert_eq!(render_status(&[], false, true)?, "[]\n");
        Ok(())
    }

    #[test]
    fn the_explain_chain_renders_unchanged() -> anyhow::Result<()> {
        let steps = [
            ExplainStep {
                index: 0,
                source: "env(TOKEN)".to_string(),
                chosen: false,
                skipped: Some("TOKEN unset".to_string()),
                hint: "export TOKEN".to_string(),
            },
            ExplainStep {
                index: 1,
                source: "exec(gh)".to_string(),
                chosen: true,
                skipped: None,
                hint: String::new(),
            },
        ];
        assert_eq!(
            render_explain(&token(), &steps, false)?,
            "//auth:token\n  \
             1. env(TOKEN)               skipped: TOKEN unset\n                            \
             → export TOKEN\n  \
             2. exec(gh)                 applies here\n"
        );
        let json: serde_json::Value =
            serde_json::from_str(&render_explain(&token(), &steps, true)?)?;
        assert_eq!(json["addr"], "//auth:token");
        assert_eq!(json["sources"][1]["chosen"], true);
        Ok(())
    }

    #[test]
    fn a_status_row_serializes_the_fix_as_data_not_prose() {
        let row = Row {
            addr: "//auth:aws".to_string(),
            state: State::NeedsSignIn,
            source: "exec(aws)".to_string(),
            expires_at: None,
            login: vec![vec![
                "aws".to_string(),
                "sso".to_string(),
                "login".to_string(),
            ]],
        };
        let v: serde_json::Value = serde_json::to_value(&row).expect("serialize");
        assert_eq!(v["state"], "needs_sign_in");
        assert_eq!(v["login"][0][0], "aws");
        // Absent rather than null: a consumer branches on presence.
        assert!(v.get("expires_at").is_none());
    }

    #[test]
    fn an_ok_row_carries_an_absolute_expiry() {
        let row = Row {
            addr: "//auth:aws".to_string(),
            state: State::Ok,
            source: "exec(aws)".to_string(),
            expires_at: Some("2026-09-08T15:41:00+00:00".to_string()),
            login: vec![],
        };
        let v: serde_json::Value = serde_json::to_value(&row).expect("serialize");
        assert_eq!(v["expires_at"], "2026-09-08T15:41:00+00:00");
        assert!(
            v.get("login").is_none(),
            "an available credential has nothing to run"
        );
    }

    #[test]
    fn the_three_states_read_the_way_the_terminal_prints_them() {
        assert_eq!(State::Ok.label(), "ok");
        assert_eq!(State::NeedsSignIn.label(), "needs sign-in");
        assert_eq!(State::Unavailable.label(), "unavailable");
    }
}
