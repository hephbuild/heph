use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;
use serde::Serialize;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::Engine;
use crate::engine::driver::sandbox::Sandbox;
use crate::engine::driver::targetdef::TargetDef;
use crate::htaddr::{self, Addr};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Show the direct def only, without applying transitive deps
    #[arg(long)]
    pub no_transitive: bool,
}

#[derive(Serialize)]
struct DefView<'a> {
    target_def: &'a TargetDef,
    applied_transitive: Option<&'a Sandbox>,
}

struct DefApp {
    engine: Arc<Engine>,
    addr: Addr,
    no_transitive: bool,
    fail_fast: bool,
}

#[async_trait]
impl App for DefApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Def {}", self.addr.format()))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Def {}", self.addr.format()))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        // `get_def` may run provider targets, recording rich failures in `rs`;
        // `finalize` prefers those over the returned error and prints on success.
        let res = if self.no_transitive {
            self.engine
                .clone()
                .get_direct_def(rs.clone(), &self.addr)
                .await
        } else {
            self.engine.clone().get_def(rs.clone(), &self.addr).await
        };
        crate::commands::errors::finalize!(ctx, rs, res, def => {
            let view = DefView {
                target_def: &def.target_def,
                applied_transitive: def.applied_transitive.as_ref(),
            };
            let mut json = serde_json::to_value(&view).context("serialize def")?;
            // `pass_env` snapshots the *host's* values into the def at parse
            // time, so printing the def verbatim published whatever the shell
            // that ran the build happened to hold — an API token as readily as
            // a `TERM`. The names are what a reader of this command is after;
            // the values are host state that was never theirs to see here.
            //
            // Masked at the print rather than in the def, so the wire form a
            // driver round-trips is untouched.
            mask_pass_env(&mut json);
            let json = serde_json::to_string_pretty(&json).context("render def")?;
            println!("{json}");
            Ok(())
        })
    }
}

/// The placeholder a masked `pass_env` value is printed as.
///
/// Says what happened rather than showing an empty string, which would read as
/// "this variable is unset" and send someone debugging the wrong thing.
const MASKED: &str = "«from host, not shown»";

/// Replace every value under a `pass_env` map, wherever it appears.
///
/// Walks rather than reaching for a known path: `pass_env` lives inside a
/// driver's opaque `raw_def`, and the whole point of that blob is that this
/// command does not know its shape.
fn mask_pass_env(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if k == "pass_env"
                    && let serde_json::Value::Object(env) = child
                {
                    for value in env.values_mut() {
                        *value = serde_json::Value::String(MASKED.to_string());
                    }
                    continue;
                }
                mask_pass_env(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                mask_pass_env(item);
            }
        }
        _ => {}
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let base = crate::engine::get_cwp()?;
    let addr = htaddr::parse_addr_with_base(args.addr.as_ref(), &base)
        .with_context(|| format!("parse {}", args.addr))?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = DefApp {
        engine,
        addr,
        no_transitive: args.no_transitive,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pass_env` snapshots host values into the def, so printing one verbatim
    /// published whatever the shell that ran the build held.
    #[test]
    fn pass_env_values_are_masked_wherever_they_appear() {
        let mut v: serde_json::Value = serde_json::from_str(
            r#"{
                "raw_def": {
                    "pass_env": {"GITHUB_TOKEN": "ghp_actual_secret", "TERM": "xterm"},
                    "env": {"DECLARED": "in the BUILD file"}
                },
                "nested": [{"pass_env": {"AWS_SECRET_ACCESS_KEY": "s3cret"}}]
            }"#,
        )
        .expect("json");

        mask_pass_env(&mut v);
        let out = serde_json::to_string(&v).expect("render");

        assert!(!out.contains("ghp_actual_secret"), "{out}");
        assert!(!out.contains("s3cret"), "{out}");
        // Even an innocuous one: this command cannot tell which is which, and
        // guessing would be the leak.
        assert!(!out.contains("xterm"), "{out}");

        // The *names* survive — they are what a reader of this command wants.
        assert!(out.contains("GITHUB_TOKEN"), "{out}");
        assert!(out.contains("AWS_SECRET_ACCESS_KEY"), "{out}");
        assert!(out.contains(MASKED), "{out}");

        // Values declared in the BUILD file are public and stay readable.
        assert!(out.contains("in the BUILD file"), "{out}");
    }
}
