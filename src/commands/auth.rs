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
    }
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
