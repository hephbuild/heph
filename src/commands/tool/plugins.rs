use crate::tui::session::SessionSlot;
use anyhow::Context;
use std::io::Write;
use std::sync::Arc;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::PluginInfo;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    #[command(flatten)]
    pub out: output::JsonArgs<PluginRow<'static>>,
}

/// One registered plugin in `heph tool plugins --json`.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct PluginRow<'a> {
    /// The plugin's name.
    name: &'a str,
    /// Where it came from: `builtin plugin "fs"`, or a cdylib's path and manifest.
    source: &'a str,
    /// The full name of its provider; `null` when it has none.
    provider: Option<&'a str>,
    /// The full name of each driver it contributed.
    drivers: &'a [String],
    /// Each function it contributed, as called from a BUILD file: `heph.<plugin>.<fn>`.
    functions: &'a [String],
    /// The full name of each runner it contributed.
    runners: &'a [String],
    /// How many hooks it registered.
    hooks: usize,
}

impl<'a> From<&'a PluginInfo> for PluginRow<'a> {
    fn from(p: &'a PluginInfo) -> Self {
        Self {
            name: &p.name,
            source: &p.source,
            provider: p.provider.as_deref(),
            drivers: &p.drivers,
            functions: &p.functions,
            runners: &p.runners,
            hooks: p.hooks,
        }
    }
}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    args: Args,
    _sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(&global, session)?;
    let plugins = engine.plugins();
    if args.out.enabled() {
        return args.out.list_iter(
            plugins.iter().map(|p| PluginRow::from(*p)),
            &output::never_cancelled,
        );
    }
    let mut out = std::io::stdout().lock();
    for line in render_text(&plugins) {
        writeln!(out, "{line}").context("writing the plugin list")?;
    }
    Ok(())
}

/// One header line per plugin (its name and where it came from), then one
/// indented line per component, under the full name it is reachable by.
fn render_text(plugins: &[&PluginInfo]) -> Vec<String> {
    let mut lines = Vec::new();
    for p in plugins {
        lines.push(format!("{}  ({})", p.name, p.source));
        let components = p
            .provider
            .iter()
            .map(|n| ("provider", n))
            .chain(p.drivers.iter().map(|n| ("driver", n)))
            .chain(p.functions.iter().map(|n| ("function", n)))
            .chain(p.runners.iter().map(|n| ("runner", n)));
        for (kind, name) in components {
            lines.push(format!("  {kind:<8}  {name}"));
        }
        if p.hooks > 0 {
            lines.push(format!("  {:<8}  {}", "hooks", p.hooks));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> PluginInfo {
        PluginInfo {
            name: "auth".to_string(),
            source: "builtin plugin \"auth\"".to_string(),
            provider: None,
            drivers: vec!["auth.credential".to_string()],
            functions: vec!["heph.auth.oidc".to_string()],
            runners: vec![],
            hooks: 0,
        }
    }

    fn go() -> PluginInfo {
        PluginInfo {
            name: "go".to_string(),
            source: "cdylib plugin /p/libgo.so (manifest /p/heph-go.json)".to_string(),
            provider: Some("go".to_string()),
            drivers: vec!["go".to_string(), "go.test".to_string()],
            functions: vec![],
            runners: vec![],
            hooks: 1,
        }
    }

    #[test]
    fn components_are_listed_by_full_name_under_their_plugin() {
        assert_eq!(
            render_text(&[&auth(), &go()]),
            vec![
                "auth  (builtin plugin \"auth\")",
                "  driver    auth.credential",
                "  function  heph.auth.oidc",
                "go  (cdylib plugin /p/libgo.so (manifest /p/heph-go.json))",
                "  provider  go",
                "  driver    go",
                "  driver    go.test",
                "  hooks     1",
            ]
        );
    }

    /// The default set is exactly the keys `PluginInfo` serialized before the
    /// command opted in (compatibility condition C3), with the same values.
    #[test]
    fn json_rows_keep_the_plugin_info_shape() {
        let (auth, go) = (auth(), go());
        let mut out = Vec::new();
        output::JsonArgs::<PluginRow<'static>>::try_parse_from(["--json"])
            .expect("--json parses")
            .printer_to(&mut out, output::Style::Compact)
            .list_iter(
                [&auth, &go].into_iter().map(PluginRow::from),
                &output::never_cancelled,
            )
            .expect("print");
        let doc: serde_json::Value = serde_json::from_slice(&out).expect("one JSON document");
        let before = serde_json::to_value([&auth, &go]).expect("PluginInfo serializes");
        assert_eq!(doc, before);
    }

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        output::check_keys(&PluginRow::from(&go()))
    }
}
