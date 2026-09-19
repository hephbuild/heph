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
use crate::engine::credential::source_login;
use crate::engine::{Engine, get_cwp};
use crate::tui::LogSink;
use anyhow::Context as _;
use clap::{Args, Subcommand};
use hbuiltins::plugincredential::{CredentialDef, DRIVER_NAME, parse_declaration};
use hmodel::htaddr::Addr;
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
    pub fn execute(&self, _sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        match &self.command {
            AuthCommands::Status(a) => bootstrap::block_on(status(a.clone(), global.clone()))?,
            AuthCommands::Explain(a) => bootstrap::block_on(explain(a.clone(), global.clone()))?,
            AuthCommands::Login(a) => bootstrap::block_on(login(a.clone(), global.clone()))?,
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

async fn status(args: StatusArgs, _global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let rs = engine.new_state();
    let creds = find_credentials(&engine, &rs, args.matcher.as_deref()).await?;

    let mut rows = Vec::with_capacity(creds.len());
    for (addr, def) in &creds {
        rows.push(row_for(&engine, &rs, addr, def).await);
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).context("render --json")?
        );
    } else if rows.is_empty() {
        println!("no `credential` targets in this workspace");
    } else {
        let widest = rows.iter().map(|r| r.addr.len()).max().unwrap_or(0);
        for r in &rows {
            let tail = match (&r.expires_at, r.login.first()) {
                (Some(at), _) => format!("expires {at}"),
                (None, Some(_)) => format!("run: heph auth login {}", r.addr),
                (None, None) => String::new(),
            };
            println!(
                "{:<widest$}  {:<14} {:<24} {tail}",
                r.addr,
                r.state.label(),
                r.source
            );
        }
    }

    // Non-zero unless every row is ok, so a caller can branch on the exit status
    // and only then parse to learn what to run.
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

async fn explain(args: ExplainArgs, _global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let rs = engine.new_state();
    let cwp = get_cwp()?;
    let addr = hmodel::htaddr::parse_addr_with_base(&args.addr, &cwp)?;
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

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "addr": addr.format(),
                "sources": steps,
            }))
            .context("render --json")?
        );
        return Ok(());
    }

    println!("{addr}");
    for s in &steps {
        match &s.skipped {
            None => println!("  {}. {:<24} applies here", s.index + 1, s.source),
            Some(reason) => {
                println!("  {}. {:<24} skipped: {reason}", s.index + 1, s.source);
                println!("{:28}→ {}", "", s.hint);
            }
        }
    }
    if !steps.iter().any(|s| s.chosen) {
        anyhow::bail!("no source applies here");
    }
    Ok(())
}

async fn login(args: LoginArgs, _global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let rs = engine.new_state();
    let creds = match &args.addr {
        Some(raw) => {
            let cwp = get_cwp()?;
            let addr = hmodel::htaddr::parse_addr_with_base(raw, &cwp)?;
            let spec = Arc::clone(&engine).get_spec(rs.clone(), &addr).await?;
            if spec.driver != DRIVER_NAME {
                anyhow::bail!("{addr} is a `{}` target, not a credential", spec.driver);
            }
            vec![(addr, parse_declaration(&spec)?)]
        }
        None => find_credentials(&engine, &rs, None).await?,
    };

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
                println!("{addr}: {}", argv.join(" "));
                // Through the engine's exec-runner seam, so a source whose tool
                // lives in a devenv shell signs in *there* — the same place its
                // probe looked. Stdio is inherited: a browser flow needs the
                // terminal when there is one, and an agent needs to see the URL
                // and code on its own output when there is not.
                let status = engine.run_login(&rs, addr, src, argv).await?;
                if !status.success() {
                    anyhow::bail!("{addr}: `{}` exited with {status}", argv.join(" "));
                }
                ran += 1;
            }
        }
    }
    if ran == 0 {
        println!("nothing to sign in to — every declared credential already applies here");
    }
    Ok(())
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

/// Every `credential` target in the workspace (or under `matcher`).
///
/// Resolves each spec, which is what makes this a preflight rather than something
/// on a build's path. There is no "by driver" matcher in the query language and
/// adding one for this would be a lot of surface for one command.
async fn find_credentials(
    engine: &Arc<Engine>,
    rs: &Arc<crate::engine::request_state::RequestState>,
    matcher: Option<&str>,
) -> anyhow::Result<Vec<(Addr, CredentialDef)>> {
    use futures::TryStreamExt as _;
    let cwp = get_cwp()?;
    // Through the expression slot, not the positional one: the single-positional
    // form takes an *address*, and the selection here is a package matcher.
    let m = crate::commands::utils::resolve_matcher(
        &Some(matcher.unwrap_or("//...").to_string()),
        &None,
        &None,
        &cwp,
        true,
    )?;
    let stream = Arc::clone(engine).query(rs.clone(), &m);
    tokio::pin!(stream);
    let mut out = Vec::new();
    while let Some(addr) = stream.try_next().await? {
        let spec = Arc::clone(engine).get_spec(rs.clone(), &addr).await?;
        if spec.driver != DRIVER_NAME {
            continue;
        }
        let def = parse_declaration(&spec).with_context(|| format!("credential {addr}"))?;
        out.push((addr, def));
    }
    out.sort_by_key(|a| a.0.format());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
