//! `heph auth` — everything credential-facing under one noun.
//!
//! Grouped rather than scattered across the top level so the surface is
//! discoverable from `heph auth --help` instead of by knowing what to look for.
//!
//! # There is deliberately no `token` subcommand
//!
//! Printing a minted credential to a terminal is how it reaches scrollback,
//! shell history, and a pasted bug report. `check` answers the question anyone
//! would reach for it to ask — *is my identity actually wired up* — by minting
//! and immediately dropping.
//!
//! # `login` is the only interactive thing here
//!
//! And it is a separate command precisely so nothing else is. A build that
//! opens a browser at target 400 of 900 is an ambush for a human and a silent
//! hang for an agent, so the provider only ever *presents* an identity that
//! already exists — and says to run this when one does not.
//!
//! # `show` is the other half of a bargain
//!
//! The design's position on caching is that a target holding a credential stays
//! cacheable and **the author configures**. That is only a fair thing to ask of
//! someone who can see what they are configuring, so `show` reports the merged
//! view for a target: every file that would be written, every variable set, and
//! which descriptor owns each entry. It never mints, so it is safe to run
//! anywhere.

use std::sync::Arc;

use anyhow::Context as _;

use crate::commands::bootstrap;
use crate::commands::{GlobalOptions, utils};
use crate::engine::{Engine, get_cwp};
use crate::htaddr::Addr;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommands,
}

#[derive(clap::Subcommand, Clone)]
pub enum AuthCommands {
    /// Show which credentials a target would hold, and what they would write
    ///
    /// The merged view: every file, every variable, and which declaration owns
    /// each entry. Never mints anything, so it is safe to run anywhere — and it
    /// is where a slot collision is seen before it is hit.
    ///
    /// Given a pattern rather than one address, lists the targets that are both
    /// credential-bearing and remotely cached, which is the combination that
    /// warrants a deliberate decision.
    Show(ShowArgs),
    /// Mint every credential a pattern touches, then drop it
    ///
    /// The "is my identity actually wired up" command, for a laptop and a CI
    /// smoke job alike. On a warm workspace it is the only thing that ever
    /// validates the credential path, since a cache hit mints nothing.
    Check(CheckArgs),
    /// Sign in to the workspace's identity provider
    ///
    /// Gives this machine a workload identity of the same kind CI gets
    /// ambiently, so one credential declaration works in both places. Opens a
    /// browser; the only command here that ever does.
    ///
    /// What is stored is a single refresh token, in a mode-0600 file under
    /// $HOME/.heph/auth. No cloud credentials, and nothing inside a workspace.
    Login(LoginArgs),
    /// Show whether this machine has a session, and whose
    Status(StatusArgs),
    /// Forget the stored session on this machine
    ///
    /// Local only: the token is deleted here, not revoked at the provider.
    Logout(LogoutArgs),
}

#[derive(clap::Args, Clone)]
pub struct LoginArgs {
    /// Print a code to enter on another device instead of opening a browser
    ///
    /// For a machine with no browser — a remote shell, a container, a CI
    /// runner being set up by hand.
    #[arg(long)]
    pub device_code: bool,
    /// Sign in again even if this machine already has a valid session
    #[arg(long)]
    pub force: bool,
    /// Emit JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone)]
pub struct StatusArgs {
    /// List every session on this machine, not just this workspace's
    ///
    /// Works offline and outside a workspace. Sessions are keyed by issuer and
    /// client id, so a session orphaned by an org rotating either one is
    /// otherwise invisible.
    #[arg(long)]
    pub all: bool,
    /// Emit JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone)]
pub struct LogoutArgs {
    /// Forget every session on this machine, not just this workspace's
    ///
    /// What a laptop cleanup actually wants, and the only way to reach a
    /// session orphaned by a changed issuer or client id.
    #[arg(long)]
    pub all: bool,
    /// Emit JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone)]
pub struct ShowArgs {
    /// Target address, or a pattern (`//svc/...`)
    #[arg(value_name = "TARGET")]
    pub target: String,
    /// Emit JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone)]
pub struct CheckArgs {
    /// Target address, or a pattern (`//...`)
    #[arg(value_name = "TARGET", default_value = "//...")]
    pub target: String,
    /// Emit JSON
    #[arg(long)]
    pub json: bool,
}

pub fn execute(args: &AuthArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    match &args.command {
        AuthCommands::Show(a) => bootstrap::block_on(show(a.clone(), sink, global.clone()))?,
        AuthCommands::Check(a) => bootstrap::block_on(check(a.clone(), sink, global.clone()))?,
        AuthCommands::Login(a) => bootstrap::block_on(login(a.clone()))?,
        AuthCommands::Status(a) => bootstrap::block_on(status(a.clone()))?,
        AuthCommands::Logout(a) => logout(a),
    }
}

/// The workspace's `auth:` block, or a message naming what to add.
///
/// A workspace with no block is not misconfigured — it may have no IdP at all —
/// so the error explains the shape rather than reporting a parse failure, and
/// names the file it looked in.
fn auth_config() -> anyhow::Result<hsecrets::AuthConfig> {
    let root = crate::engine::get_root()?;
    let path = root.join(".hephconfig");
    crate::engine::config_yaml::load_from_root(&root)?
        .auth
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                hsecrets::session::no_auth_block(&path.display().to_string())
            )
        })
}

/// Everything a build or the CLI can conclude about this machine's identity.
#[derive(Debug)]
enum State {
    /// CI handed this process a workload identity. Nothing local is needed, and
    /// nothing local would be used — an ambient identity is scoped to the job,
    /// so it wins whenever both exist.
    Ambient(&'static str),
    /// A session that was just proved to still work.
    Active(Box<hsecrets::Session>),
    /// A session file exists and the IdP will not renew it.
    Expired,
    /// There is a session, and we could not find out whether it works.
    ///
    /// Distinct from `Expired` because the instruction differs: on a plane,
    /// telling someone their session was revoked sends them to a browser that
    /// also cannot reach the IdP. `is_invalid_grant` is exactly what separates
    /// the two, and the build path already uses it.
    Unknown(String),
    None,
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            Self::Ambient(_) => "ambient",
            Self::Active(_) => "active",
            Self::Expired => "expired",
            Self::Unknown(_) => "unknown",
            Self::None => "none",
        }
    }
}

/// Answer "does this machine have a usable identity", the way a build answers it.
///
/// Ambient first, then the session — and the session is *probed*, not merely
/// read. A stored file proves nothing: most IdPs never send
/// `refresh_expires_in`, so a revoked grant looks identical on disk to a live
/// one. Reporting "signed in" from the file alone is how every path ends up
/// telling the user to run a command that then says there is nothing to do.
async fn resolve_state(cfg: &hsecrets::AuthConfig, home: &std::path::Path) -> State {
    if let Some(source) = hsecrets::oidc::AmbientIdentity::detect(&|k| std::env::var(k).ok()) {
        return State::Ambient(source.source);
    }
    match hsecrets::Session::load(home, &cfg.issuer, &cfg.client_id) {
        Ok(None) => return State::None,
        // A file that will not parse is not "never signed in": `load` built a
        // diagnostic naming the file and the fix, and throwing it away is how
        // a corrupt session becomes a mystery.
        Err(e) => return State::Unknown(format!("{e:#}")),
        Ok(Some(_)) => {}
    }

    let client = match hsecrets::session::http_client() {
        Ok(c) => c,
        Err(e) => return State::Unknown(format!("{e:#}")),
    };

    // Bounded, because this runs in a shell prompt and in setup scripts: the
    // lock's blocking acquire waits without limit on a peer that holds it, and
    // `status` must not be the command that hangs.
    let ctoken = hcore::hasync::StdCancellationToken::new();
    let probe = hsecrets::session::refresh_locked(
        &client,
        cfg,
        home,
        None,
        std::time::SystemTime::now(),
        &ctoken,
    );
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(_)) => match hsecrets::Session::load(home, &cfg.issuer, &cfg.client_id) {
            Ok(Some(s)) => State::Active(Box::new(s)),
            Ok(None) => State::None,
            Err(e) => State::Unknown(format!("{e:#}")),
        },
        // The one case where "log in again" is the right advice.
        Ok(Err(e)) if hsecrets::session::is_invalid_grant(&e) => State::Expired,
        Ok(Err(e)) => State::Unknown(format!("{e:#}")),
        Err(_elapsed) => State::Unknown(format!(
            "the identity provider did not answer within {}s",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

/// How long `status` and `login` will wait to find out whether a session still
/// works before reporting that they could not.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(serde::Serialize)]
struct SessionView {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// Whether this invocation changed anything — so an agent can tell a fresh
    /// sign-in from a no-op without diffing.
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<bool>,
}

impl SessionView {
    fn of(state: &State, cfg: &hsecrets::AuthConfig, home: &std::path::Path) -> Self {
        let base = Self {
            state: state.name(),
            source: None,
            issuer: Some(cfg.issuer.clone()),
            client_id: Some(cfg.client_id.clone()),
            subject: None,
            expires_at: None,
            path: None,
            changed: None,
        };
        match state {
            State::Ambient(source) => Self {
                source: Some(source),
                // An ambient identity has nothing to do with the workspace's
                // IdP, so claiming its issuer would be a lie.
                issuer: None,
                client_id: None,
                ..base
            },
            State::Active(s) => Self {
                subject: s.subject.clone(),
                expires_at: s.expires_at.map(rfc3339),
                path: Some(
                    hsecrets::Session::path(home, &cfg.issuer, &cfg.client_id)
                        .display()
                        .to_string(),
                ),
                ..base
            },
            State::Unknown(why) => Self {
                // The reason, so an agent does not have to guess whether to
                // retry or to re-authenticate.
                subject: Some(why.clone()),
                ..base
            },
            State::Expired | State::None => base,
        }
    }
}

fn rfc3339(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Under `--json`, stdout carries JSON and nothing else.
///
/// Progress prose on stdout is what makes `heph auth login --json | jq` a parse
/// error, and this feature's second audience is an agent doing exactly that.
fn say(json: bool, msg: &str) {
    if json {
        eprintln!("{msg}");
    } else {
        println!("{msg}");
    }
}

fn emit(view: &SessionView) -> anyhow::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(view).context("render json")?
    );
    Ok(())
}

async fn login(args: LoginArgs) -> anyhow::Result<()> {
    let cfg = auth_config()?;
    let home = hsecrets::Session::home()?;
    let now = std::time::SystemTime::now();

    // A second sign-in is a wasted browser round trip and, on some IdPs, a
    // burned rate limit. `--force` exists for the case where the *scopes*
    // changed and the old session is valid but wrong.
    if !args.force {
        match resolve_state(&cfg, &home).await {
            State::Ambient(source) => {
                say(
                    args.json,
                    &format!(
                        "this machine already has an ambient workload identity ({source}); a \
                         local session is not needed here"
                    ),
                );
                if args.json {
                    emit(&SessionView::of(&State::Ambient(source), &cfg, &home))?;
                }
                return Ok(());
            }
            State::Active(s) => {
                if args.json {
                    let mut view = SessionView::of(&State::Active(s), &cfg, &home);
                    view.changed = Some(false);
                    return emit(&view);
                }
                println!("already signed in to {}{}", cfg.issuer, whom(&s));
                println!("run `heph auth login --force` to sign in again");
                return Ok(());
            }
            // Not knowing is a reason to sign in, not a reason to refuse:
            // whatever the obstacle, the browser flow is the way through it,
            // and the reason is worth saying first.
            State::Unknown(why) => {
                tracing::warn!(issuer = %cfg.issuer, "could not check the existing session: {why}");
            }
            // Both fall through to the browser. Expired is the whole point:
            // the previous behaviour trusted the file, so a revoked grant read
            // as "already signed in" and every path told the user to run the
            // one command that refused to do anything.
            State::Expired | State::None => {}
        }
    }

    // Said before the browser opens, not after: a login that succeeds and then
    // cannot be stored is the confusing outcome.
    for w in cfg.warnings() {
        tracing::warn!(issuer = %cfg.issuer, "{w}");
    }

    let client = hsecrets::session::http_client()?;
    let meta = hsecrets::Metadata::discover(&client, &cfg.issuer).await?;
    if meta.lacks_s256() {
        tracing::warn!(
            issuer = %cfg.issuer,
            "the issuer does not advertise PKCE S256; signing in anyway, since the list is \
             advisory — but if the provider rejects the request, that is why"
        );
    }

    let session = if args.device_code {
        let auth = hsecrets::session::device_start(&client, &cfg, &meta).await?;
        if args.json {
            // Newline-delimited, and emitted *before* the wait: a code that
            // only arrives after it has expired is no use to an agent showing
            // it to a person.
            println!(
                "{}",
                serde_json::json!({
                    "event": "device_code",
                    "verification_uri": auth.verification_uri,
                    "verification_uri_complete": auth.verification_uri_complete,
                    "user_code": auth.user_code,
                    "expires_in": auth.expires_in,
                })
            );
        } else {
            // The pre-filled URL where the IdP offers one: a user code typed by
            // hand is a user code typed wrong.
            match &auth.verification_uri_complete {
                Some(url) => println!("open {url}"),
                None => println!(
                    "open {} and enter the code: {}",
                    auth.verification_uri, auth.user_code
                ),
            }
            println!("waiting…");
        }
        hsecrets::session::device_poll(&client, &cfg, &meta, &auth, now).await?
    } else {
        if let Some(hint) = headless_hint() {
            say(args.json, &hint);
        }
        say(
            args.json,
            &format!("opening your browser to sign in to {}", cfg.issuer),
        );
        hsecrets::session::login(&client, &cfg, &meta, now, |url, redirect| {
            // Both printed before the wait. The redirect URI is the usual cause
            // of a failure the user sees in three seconds and heph would
            // otherwise only diagnose after the three-minute timeout.
            say(
                args.json,
                &format!(
                    "waiting for the callback on {redirect}\n  this exact URI must be \
                     registered as a redirect URI for client {}",
                    cfg.client_id
                ),
            );
            // Always printed as well as opened: on a machine where the browser
            // cannot be launched this is the whole flow, not a fallback.
            say(args.json, &format!("if it does not open, go to:\n  {url}"));
            hsecrets::session::open_in_browser(url);
        })
        .await?
    };

    session.store(&home)?;
    if args.json {
        let mut view = SessionView::of(&State::Active(Box::new(session)), &cfg, &home);
        view.changed = Some(true);
        return emit(&view);
    }
    println!("signed in to {}{}", session.issuer, whom(&session));
    Ok(())
}

/// ` as alice@org.example`, or nothing when the IdP told us no subject.
fn whom(s: &hsecrets::Session) -> String {
    s.subject
        .as_deref()
        .map(|x| format!(" as {x}"))
        .unwrap_or_default()
}

/// Whether this looks like a machine whose browser is somewhere else.
///
/// Driven by the environment rather than by `cfg!`, so the behaviour is the
/// same on all three targets: a browser on your laptop cannot reach
/// `127.0.0.1` on the box you SSH'd into, and the loopback flow will simply
/// time out with no clue as to why.
fn headless_hint() -> Option<String> {
    let remote =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let no_display = cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none();
    headless_hint_for(remote, no_display)
}

/// The predicate, separated from the environment read so it is testable
/// without mutating the process environment — which parallel tests share.
fn headless_hint_for(remote: bool, no_display: bool) -> Option<String> {
    (remote || no_display).then(|| {
        "note: this looks like a machine with no local browser. A browser elsewhere cannot reach \
         127.0.0.1 here — run `heph auth login --device-code` instead."
            .to_string()
    })
}

async fn status(args: StatusArgs) -> anyhow::Result<()> {
    let home = hsecrets::Session::home()?;
    if args.all {
        return status_all(&home, args.json);
    }

    let cfg = auth_config()?;
    let state = resolve_state(&cfg, &home).await;
    if args.json {
        emit(&SessionView::of(&state, &cfg, &home))?;
    }

    match &state {
        State::Ambient(source) => {
            if !args.json {
                println!("identity: ambient ({source})");
                println!("  no local session is needed here");
            }
            Ok(())
        }
        State::Active(s) => {
            if !args.json {
                println!("signed in to {}{}", cfg.issuer, whom(s));
                println!(
                    "  {}",
                    hsecrets::Session::path(&home, &cfg.issuer, &cfg.client_id).display()
                );
            }
            Ok(())
        }
        // Not signed in is a *state*, not a crash: the instruction goes to
        // stdout with everything else, and the non-zero exit is what lets a
        // setup script branch on it without parsing prose.
        rest => {
            if !args.json {
                match rest {
                    State::Expired => {
                        println!("the session for {} is no longer valid", cfg.issuer);
                        println!("  run `heph auth login`");
                    }
                    // Deliberately not "run `heph auth login`": on a plane that
                    // sends the user to a browser that cannot reach the IdP
                    // either.
                    State::Unknown(why) => {
                        println!("there is a session for {}, but it could not be", cfg.issuer);
                        println!("  checked: {why}");
                    }
                    _ => {
                        println!("not signed in to {}", cfg.issuer);
                        println!("  run `heph auth login`");
                    }
                }
            }
            Err(anyhow::Error::new(crate::commands::errors::QuietExit))
        }
    }
}

/// Every session on this machine, whatever workspace put it there.
///
/// The reason this exists: sessions are keyed by `(issuer, client_id)`, so the
/// day an org rotates its client id the old file becomes unreachable through
/// the workspace config — and it still holds a live refresh token. Without a
/// way to enumerate and delete it, that is a credential with no exit.
type OnDisk = (std::path::PathBuf, Option<hsecrets::Session>);

fn sessions_on_disk(home: &std::path::Path) -> anyhow::Result<Vec<OnDisk>> {
    let dir = home.join("auth");
    let entries = match std::fs::read_dir(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other.with_context(|| format!("read {}", dir.display()))?,
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("read {}", dir.display()))?
            .path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        // Every `*.json` here is listed, parseable or not. A file written by a
        // schema change, or torn by an interrupted write, is *exactly* the one
        // that is unreachable through the workspace config — so skipping it
        // would leave a live refresh token that `--all` reports as absent.
        let session = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<hsecrets::Session>(&b).ok());
        out.push((path, session));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn status_all(home: &std::path::Path, json: bool) -> anyhow::Result<()> {
    let found = sessions_on_disk(home)?;
    if json {
        let views: Vec<_> = found
            .iter()
            .map(|(path, s)| {
                serde_json::json!({
                    "issuer": s.as_ref().map(|s| s.issuer.clone()),
                    "client_id": s.as_ref().map(|s| s.client_id.clone()),
                    "subject": s.as_ref().and_then(|s| s.subject.clone()),
                    "readable": s.is_some(),
                    "path": path.display().to_string(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&views).context("render json")?
        );
        return Ok(());
    }
    if found.is_empty() {
        println!("no sessions on this machine");
        return Ok(());
    }
    // Deliberately not probed: this is the cleanup view, and it must work
    // offline and outside a workspace.
    for (path, s) in &found {
        match s {
            Some(s) => println!("{}{}", s.issuer, whom(s).replace(" as ", "  ")),
            // Named rather than hidden: this is the one `--all` exists to reach.
            None => println!("<unreadable>"),
        }
        println!("  {}", path.display());
    }
    Ok(())
}

fn logout(args: &LogoutArgs) -> anyhow::Result<()> {
    let home = hsecrets::Session::home()?;

    let removed = if args.all {
        // By path, so a file that will not parse — which is precisely the one
        // no other command can reach — is removed rather than reported absent.
        let found = sessions_on_disk(&home)?;
        for (path, _) in &found {
            std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        }
        found.len()
    } else {
        let cfg = auth_config()?;
        usize::from(hsecrets::Session::forget(
            &home,
            &cfg.issuer,
            &cfg.client_id,
        )?)
    };

    if args.json {
        println!("{}", serde_json::json!({ "removed": removed }));
    } else if removed > 0 {
        println!("removed {removed} session(s)");
        // Said plainly, because "logged out" would be a false claim: the grant
        // is still live at the provider until it is revoked there.
        println!("the refresh token is deleted here, not revoked at the provider");
    } else {
        println!("no session to remove");
    }
    Ok(())
}

/// One credential a target holds, as `show` reports it.
#[derive(serde::Serialize)]
struct Held {
    name: String,
    secret: String,
    /// The dependency chain that supplied it; empty when declared directly.
    via: Vec<String>,
    shapes: Vec<String>,
    /// Where each shape writes, and under which key.
    slots: Vec<String>,
}

#[derive(serde::Serialize)]
struct ShowView {
    target: String,
    remote_cached: bool,
    subject_scoped: bool,
    secrets: Vec<Held>,
}

async fn show(args: ShowArgs, _sink: LogSink, _global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let addrs = resolve(&engine, &args.target).await?;

    let mut views = Vec::new();
    for addr in addrs {
        let view = describe(&engine, &addr).await?;
        // A pattern lists only what is interesting; one address always reports,
        // so `heph auth show //x:y` never answers with silence.
        if view.secrets.is_empty() && args.target.contains("...") {
            continue;
        }
        views.push(view);
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&views).context("render json")?
        );
        return Ok(());
    }

    if views.is_empty() {
        println!("no targets hold credentials");
        return Ok(());
    }

    for v in &views {
        println!("{}", v.target);
        // The combination the design asks an author to have looked at: a
        // credential-bearing target whose output is shared with everyone who can
        // reach the cache.
        if v.remote_cached && !v.subject_scoped && !v.secrets.is_empty() {
            println!(
                "  remotely cached — whatever this produced is served to anyone who can reach \
                 the cache"
            );
        }
        if v.subject_scoped {
            println!("  subject-scoped — keyed by who ran the build");
        }
        for s in &v.secrets {
            let via = if s.via.is_empty() {
                "declared".to_string()
            } else {
                format!("via {}", s.via.join(" → "))
            };
            println!("  {:<12} {:<28} {via}", s.name, s.secret);
            for slot in &s.slots {
                println!("      {slot}");
            }
        }
        println!();
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct CheckResult {
    secret: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn check(args: CheckArgs, _sink: LogSink, _global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let addrs = resolve(&engine, &args.target).await?;
    let rs = engine.new_state();

    // Deduped by descriptor: a workspace where two hundred targets name one
    // credential should make one attempt, not two hundred.
    let mut seen = std::collections::BTreeSet::new();
    let mut descs = Vec::new();
    for addr in &addrs {
        let def = match Arc::clone(&engine).get_def(rs.clone(), addr).await {
            Ok(d) => d,
            // A target that does not resolve is not this command's problem to
            // report — `heph build` says it better.
            Err(_) => continue,
        };
        for r in engine
            .resolve_secrets_for_check(&rs, addr, &def.target_def.inputs)
            .await?
        {
            if seen.insert(r.desc.addr.clone()) {
                descs.push(r);
            }
        }
    }

    if descs.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("no credentials to check");
        }
        return Ok(());
    }

    let mut results = Vec::new();
    let mut failed = 0usize;
    for r in &descs {
        // Minted and dropped: the value is never written, never rendered, and
        // never printed. What is reported is whether the route worked.
        let res = engine.mint_for_check(&rs, r).await;
        let ok = res.is_ok();
        if !ok {
            failed = failed.saturating_add(1);
        }
        results.push(CheckResult {
            secret: r.desc.addr.clone(),
            ok,
            error: res.err().map(|e| format!("{e:#}")),
        });
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results).context("render json")?
        );
    } else {
        for r in &results {
            if r.ok {
                println!("ok    {}", r.secret);
            } else {
                println!("FAIL  {}", r.secret);
                for line in r.error.as_deref().unwrap_or_default().lines() {
                    println!("        {line}");
                }
            }
        }
    }

    if failed > 0 {
        anyhow::bail!(
            "{failed} of {} credentials could not be obtained",
            results.len()
        );
    }
    Ok(())
}

/// One address, or every target a pattern selects.
async fn resolve(engine: &Arc<Engine>, target: &str) -> anyhow::Result<Vec<Addr>> {
    if target.contains("...") || target.contains("&&") || target.contains('+') {
        let matcher = crate::htquery::parse(target, &get_cwp()?)?;
        let rs = engine.new_state();
        use futures::TryStreamExt as _;
        let addrs: Vec<Addr> = Arc::clone(engine).query(rs, &matcher).try_collect().await?;
        Ok(addrs)
    } else {
        Ok(vec![utils::resolve_addr(target)?])
    }
}

async fn describe(engine: &Arc<Engine>, addr: &Addr) -> anyhow::Result<ShowView> {
    let rs = engine.new_state();
    let def = Arc::clone(engine).get_def(rs.clone(), addr).await?;
    let held = engine
        .resolve_secrets_for_check(&rs, addr, &def.target_def.inputs)
        .await?;

    let mut secrets = Vec::new();
    for r in held {
        let mut slots = Vec::new();
        for shape_name in &r.desc.identity.shape {
            let shape = hsecrets::shape::Shape::parse(shape_name)?;
            for slot in shape.slots(&r.name, &r.desc.identity)? {
                let where_ = shape
                    .home_path()
                    .map(|p| format!("$HOME/{p}"))
                    .unwrap_or_else(|| match shape {
                        hsecrets::shape::Shape::File => {
                            format!("${}", hdriver_support::secret::default_env_name(&r.name))
                        }
                        _ => "the environment".to_string(),
                    });
                slots.push(format!("{slot:<28} → {where_}"));
            }
        }
        secrets.push(Held {
            name: r.name.clone(),
            secret: r.desc.addr.clone(),
            via: r.via.clone(),
            shapes: r.desc.identity.shape.clone(),
            slots,
        });
    }

    Ok(ShowView {
        target: addr.format(),
        remote_cached: def.target_def.cache.remote_enabled,
        subject_scoped: def.target_def.cache.subject_scoped,
        secrets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> hsecrets::AuthConfig {
        hsecrets::AuthConfig {
            issuer: "https://org.example/oauth2/default".into(),
            client_id: "client1".into(),
            scopes: vec!["openid".into(), "offline_access".into()],
            audience: None,
            redirect_ports: Vec::new(),
        }
    }

    fn session(issuer: &str, client_id: &str, subject: &str) -> hsecrets::Session {
        hsecrets::Session {
            issuer: issuer.into(),
            client_id: client_id.into(),
            refresh_token: "rt".into(),
            subject: Some(subject.into()),
            expires_at: None,
            updated_at: None,
        }
    }

    /// `--json` owns stdout, so every outcome has to be an object — including
    /// the two that used to print a bare `null` and nothing at all. An agent
    /// parsing this cannot branch on prose.
    #[test]
    fn every_state_renders_as_a_json_object_naming_it() {
        let home = std::path::Path::new("/nonexistent");
        let c = cfg();
        for (state, want) in [
            (State::Ambient("github actions"), "ambient"),
            (
                State::Active(Box::new(session(&c.issuer, &c.client_id, "alice"))),
                "active",
            ),
            (State::Expired, "expired"),
            (State::None, "none"),
        ] {
            let v = serde_json::to_value(SessionView::of(&state, &c, home)).expect("json");
            assert!(v.is_object(), "{want} rendered as {v}");
            assert_eq!(v["state"], want, "{v}");
        }
    }

    /// An ambient identity has nothing to do with the workspace's IdP, so
    /// reporting that issuer next to it would be a lie — and it is exactly the
    /// lie a CI debugging session would act on.
    #[test]
    fn an_ambient_state_claims_no_issuer() {
        let v = serde_json::to_value(SessionView::of(
            &State::Ambient("github actions"),
            &cfg(),
            std::path::Path::new("/nonexistent"),
        ))
        .expect("json");
        assert_eq!(v["source"], "github actions");
        assert!(v.get("issuer").is_none(), "{v}");
        assert!(v.get("client_id").is_none(), "{v}");
    }

    /// The message a workspace with no `auth:` block gets. It is shared with
    /// the build path, so it has to carry both routes and name the file.
    #[test]
    fn the_missing_block_message_names_the_file_and_both_routes() {
        let msg = hsecrets::session::no_auth_block("/repo/.hephconfig");
        assert!(msg.contains("/repo/.hephconfig"), "{msg}");
        assert!(msg.contains("heph auth login"), "{msg}");
        assert!(msg.contains("id-token: write"), "{msg}");
        assert!(msg.contains("acquire"), "{msg}");
        // The substitution has to actually happen, or the placeholder ships.
        assert!(!msg.contains("{path}"), "{msg}");
    }

    /// Not knowing whether a session works is not the same as knowing it is
    /// dead. Conflating them tells someone on a plane their session was
    /// revoked, and sends them to a browser that cannot reach the IdP either.
    #[test]
    fn an_unreachable_provider_is_not_reported_as_a_revoked_session() {
        let v = serde_json::to_value(SessionView::of(
            &State::Unknown("dns error".into()),
            &cfg(),
            std::path::Path::new("/nonexistent"),
        ))
        .expect("json");
        assert_eq!(v["state"], "unknown");
        // The reason travels with it, so an agent can decide between retrying
        // and re-authenticating.
        assert!(v.to_string().contains("dns error"), "{v}");
    }

    /// Sessions are keyed by `(issuer, client_id)`, so one orphaned by an org
    /// rotating either is unreachable through the workspace config — and still
    /// holds a live refresh token. `--all` is the only way out.
    #[test]
    fn every_session_on_disk_is_enumerable_regardless_of_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        session("https://a.example", "old-client", "alice")
            .store(dir.path())
            .expect("store a");
        session("https://b.example", "new-client", "alice")
            .store(dir.path())
            .expect("store b");
        // Not a session; it must not derail the listing.
        std::fs::write(dir.path().join("auth").join("notes.txt"), b"x").expect("write");

        // The case `--all` exists for: a file written by a schema change, or
        // torn by an interrupted write. It is the one no other command can
        // reach, so listing must not skip it.
        std::fs::write(dir.path().join("auth").join("torn.json"), b"{").expect("write");

        let found = sessions_on_disk(dir.path()).expect("list");
        assert_eq!(found.len(), 3, "{found:?}");
        let issuers: Vec<_> = found
            .iter()
            .filter_map(|(_, s)| s.as_ref().map(|s| s.issuer.as_str()))
            .collect();
        assert!(issuers.contains(&"https://a.example"), "{issuers:?}");
        assert!(issuers.contains(&"https://b.example"), "{issuers:?}");
        assert_eq!(
            found.iter().filter(|(_, s)| s.is_none()).count(),
            1,
            "the unreadable file was skipped, so `--all` could not remove it"
        );
    }

    /// A machine nobody has signed in on is not an error to enumerate.
    #[test]
    fn enumerating_sessions_on_a_fresh_machine_is_empty_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(sessions_on_disk(dir.path()).expect("no error").is_empty());
    }

    #[test]
    fn logging_out_of_everything_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        session("https://a.example", "c1", "alice")
            .store(dir.path())
            .expect("store");
        session("https://b.example", "c2", "alice")
            .store(dir.path())
            .expect("store");

        std::fs::write(dir.path().join("auth").join("torn.json"), b"{").expect("write");

        for (path, _) in sessions_on_disk(dir.path()).expect("list") {
            std::fs::remove_file(&path).expect("remove");
        }
        // Including the unreadable one: leaving a live refresh token behind
        // after `logout --all` is the failure this command exists to prevent.
        assert!(sessions_on_disk(dir.path()).expect("list").is_empty());
    }

    /// A browser on your laptop cannot reach `127.0.0.1` on the box you SSH'd
    /// into, and the loopback flow would otherwise just time out with no clue.
    #[test]
    fn a_remote_shell_is_told_about_the_device_flow() {
        // Env-driven rather than `cfg!`, so this reads the same on all three
        // supported targets; the test drives the predicate, not the process.
        assert!(
            headless_hint_for(true, false)
                .expect("remote")
                .contains("--device-code")
        );
        assert!(headless_hint_for(false, true).is_some());
        assert!(headless_hint_for(false, false).is_none());
    }

    /// A subject the IdP did not give is absence, not an empty ` as `.
    #[test]
    fn a_session_without_a_subject_reads_cleanly() {
        let mut s = session("https://a.example", "c", "alice");
        assert_eq!(whom(&s), " as alice");
        s.subject = None;
        assert_eq!(whom(&s), "");
    }
}
