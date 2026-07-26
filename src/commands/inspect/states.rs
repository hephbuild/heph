use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::{Engine, PackageStates, StatesOptions};
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::htvalue::Value;
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Package matcher (defaults to all packages)
    pub matcher: Option<String>,

    /// Also show the states a package inherits from its ancestors
    // No short form on purpose: `-i` is `--interactive` on the sibling `deps`
    // command, and quietly meaning something else here would bite.
    #[arg(long)]
    pub inherited: bool,

    /// Only show states addressed to this provider
    #[arg(short, long)]
    pub provider: Option<String>,

    /// Emit JSON instead of the text listing
    #[arg(long)]
    pub json: bool,
}

struct StatesApp {
    engine: Arc<Engine>,
    matcher: Matcher,
    opts: StatesOptions,
    json: bool,
    fail_fast: bool,
}

#[async_trait]
impl App for StatesApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("States {:?}", self.matcher))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("States {:?}", self.matcher))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());

        // Probing runs BUILD evaluation, which records rich failures in `rs`;
        // `finalize` prefers those over the returned error.
        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            let found = Arc::clone(&self.engine)
                .states(rs.clone(), &self.matcher, &self.opts)
                .await?;
            // A package with nothing to say is noise in a workspace-wide scan,
            // and "absent" already reads as "no state here" for a single one.
            let found: Vec<&PackageStates> =
                found.iter().filter(|p| !p.states.is_empty()).collect();

            if self.json {
                out.println(render_json(&found)?);
            } else {
                for line in render_text(&found, self.opts.inherited) {
                    out.println(line);
                }
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res)
    }
}

/// Render a state value as JSON. BUILD files spell booleans `True`, but every
/// consumer of this output (a human grepping, an agent parsing) is better served
/// by one unambiguous encoding than by a Starlark round-trip.
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Uint(u) => serde_json::Value::from(*u),
        // A non-finite float has no JSON encoding; `serde_json::Number` rejects
        // it, so fall back to null rather than dropping the field entirely.
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Null() => serde_json::Value::Null,
        Value::List(l) => serde_json::Value::Array(l.iter().map(value_to_json).collect()),
        // Sorted so repeat invocations diff cleanly — `state` is a HashMap.
        Value::Map(m) => {
            let mut obj = serde_json::Map::with_capacity(m.len());
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for k in keys {
                obj.insert(k.clone(), value_to_json(&m[k]));
            }
            serde_json::Value::Object(obj)
        }
    }
}

fn state_fields(state: &std::collections::HashMap<String, Value>) -> String {
    let mut keys: Vec<&String> = state.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|k| format!("{k}={}", value_to_json(&state[k])))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fmt_pkg(pkg: &PkgBuf) -> String {
    format!("//{}", pkg.as_str())
}

/// One block per package: a `//pkg` header, then one line per state. With
/// `inherited`, each line is prefixed with the package that declared it — that
/// column is the whole point of the mode, so it stays even when it repeats the
/// header.
fn render_text(found: &[&PackageStates], inherited: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, ps) in found.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        lines.push(fmt_pkg(&ps.package));

        let origin_width = if inherited {
            ps.states
                .iter()
                .map(|s| fmt_pkg(&s.package).len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        let provider_width = ps
            .states
            .iter()
            .map(|s| s.provider.len())
            .max()
            .unwrap_or(0);

        for state in &ps.states {
            let mut line = String::from("  ");
            if inherited {
                line.push_str(&format!(
                    "{:<origin_width$}  ",
                    fmt_pkg(&state.package),
                    origin_width = origin_width
                ));
            }
            line.push_str(&format!(
                "{:<provider_width$}",
                state.provider,
                provider_width = provider_width
            ));
            let fields = state_fields(&state.state);
            if !fields.is_empty() {
                line.push_str("  ");
                line.push_str(&fields);
            }
            lines.push(line);
        }
    }
    lines
}

fn render_json(found: &[&PackageStates]) -> anyhow::Result<String> {
    let out: Vec<serde_json::Value> = found
        .iter()
        .map(|ps| {
            let states: Vec<serde_json::Value> = ps
                .states
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "declared_in": fmt_pkg(&s.package),
                        "provider": s.provider,
                        "state": value_to_json(&Value::Map(s.state.clone())),
                    })
                })
                .collect();
            serde_json::json!({ "package": fmt_pkg(&ps.package), "states": states })
        })
        .collect();
    serde_json::to_string_pretty(&out).context("serialize states")
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let matcher = match &args.matcher {
        Some(s) => htpkg::parse(s.as_str(), &crate::engine::get_cwp()?)?,
        None => Matcher::PackagePrefix(PkgBuf::from("")),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = StatesApp {
        engine,
        matcher,
        opts: StatesOptions {
            inherited: args.inherited,
            provider: args.provider.clone(),
        },
        json: args.json,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::provider::State;
    use std::collections::HashMap;

    fn state(pkg: &str, provider: &str, fields: &[(&str, Value)]) -> State {
        State {
            package: PkgBuf::from(pkg),
            provider: provider.to_string(),
            state: fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn fields_are_sorted_and_json_encoded() {
        let s: HashMap<String, Value> = [
            ("root".to_string(), Value::String("src".to_string())),
            ("strict".to_string(), Value::Bool(true)),
            (
                "flags".to_string(),
                Value::List(vec![Value::String("-race".to_string())]),
            ),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            state_fields(&s),
            r#"flags=["-race"] root="src" strict=true"#
        );
    }

    #[test]
    fn nested_map_keys_are_sorted_for_stable_diffs() {
        let inner: HashMap<String, Value> = [
            ("b".to_string(), Value::Int(2)),
            ("a".to_string(), Value::Int(1)),
        ]
        .into_iter()
        .collect();
        let s: HashMap<String, Value> = [("test".to_string(), Value::Map(inner))]
            .into_iter()
            .collect();

        assert_eq!(state_fields(&s), r#"test={"a":1,"b":2}"#);
    }

    #[test]
    fn text_omits_the_origin_column_without_inherited() {
        let ps = PackageStates {
            package: PkgBuf::from("a/b"),
            states: vec![state("a/b", "go", &[("strict", Value::Bool(true))])],
        };

        assert_eq!(
            render_text(&[&ps], false),
            vec!["//a/b".to_string(), "  go  strict=true".to_string()]
        );
    }

    #[test]
    fn text_shows_where_each_inherited_state_was_declared() {
        let ps = PackageStates {
            package: PkgBuf::from("a/b"),
            states: vec![
                state("", "go", &[("root", Value::String("src".to_string()))]),
                state("a/b", "exec", &[("strict", Value::Bool(true))]),
            ],
        };

        // Origin and provider columns are padded to the widest entry so the
        // fields line up down the block.
        assert_eq!(
            render_text(&[&ps], true),
            vec![
                "//a/b".to_string(),
                r#"  //     go    root="src""#.to_string(),
                "  //a/b  exec  strict=true".to_string(),
            ]
        );
    }

    #[test]
    fn packages_are_separated_by_a_blank_line() {
        let a = PackageStates {
            package: PkgBuf::from("a"),
            states: vec![state("a", "go", &[])],
        };
        let b = PackageStates {
            package: PkgBuf::from("b"),
            states: vec![state("b", "go", &[])],
        };

        assert_eq!(
            render_text(&[&a, &b], false),
            vec![
                "//a".to_string(),
                "  go".to_string(),
                String::new(),
                "//b".to_string(),
                "  go".to_string(),
            ]
        );
    }

    #[test]
    fn json_carries_the_declaring_package_per_state() -> anyhow::Result<()> {
        let ps = PackageStates {
            package: PkgBuf::from("a/b"),
            states: vec![state("a", "go", &[("strict", Value::Bool(true))])],
        };

        let parsed: serde_json::Value = serde_json::from_str(&render_json(&[&ps])?)?;
        assert_eq!(parsed[0]["package"], "//a/b");
        assert_eq!(parsed[0]["states"][0]["declared_in"], "//a");
        assert_eq!(parsed[0]["states"][0]["provider"], "go");
        assert_eq!(parsed[0]["states"][0]["state"]["strict"], true);
        Ok(())
    }
}
