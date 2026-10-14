use anyhow::Context;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::PluginInfo;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Emit JSON instead of the text listing
    #[arg(long)]
    pub json: bool,
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, _sink: LogSink, _global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let plugins = engine.plugins();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&plugins).context("encoding the plugin list")?
        );
    } else {
        for line in render_text(&plugins) {
            println!("{line}");
        }
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

    #[test]
    fn components_are_listed_by_full_name_under_their_plugin() {
        let auth = PluginInfo {
            name: "auth".to_string(),
            source: "builtin plugin \"auth\"".to_string(),
            provider: None,
            drivers: vec!["auth.credential".to_string()],
            functions: vec!["heph.auth.oidc".to_string()],
            runners: vec![],
            hooks: 0,
        };
        let go = PluginInfo {
            name: "go".to_string(),
            source: "cdylib plugin /p/libgo.so (manifest /p/heph-go.json)".to_string(),
            provider: Some("go".to_string()),
            drivers: vec!["go".to_string(), "go.test".to_string()],
            functions: vec![],
            runners: vec![],
            hooks: 1,
        };
        assert_eq!(
            render_text(&[&auth, &go]),
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
}
