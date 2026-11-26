use crate::tui::session::SessionSlot;
use anyhow::Context;
use clap::Args as ClapArgs;
use std::io::Write;
use std::sync::Arc;

use crate::commands::{GlobalOptions, bootstrap};
use crate::pluginbuildfile::LspExplanation;
use crate::tui::LogSink;

/// Hidden developer command: run the BUILD-file language server over stdio.
///
/// Editors launch this directly (`heph tool build-lsp`). It
/// speaks LSP JSON-RPC on stdin/stdout and runs until the client disconnects.
#[derive(ClapArgs, Clone)]
// The output flags describe `--explain`'s answer; the server speaks JSON-RPC
// already, so without `--explain` they are refused.
#[command(mut_group("output", |g| g.requires("explain")))]
pub struct Args {
    /// Instead of serving, print how the server classifies one position of a
    /// BUILD file: the site, why heph's answer there would be empty, and what
    /// a present answer could not do in full. Line and column are 1-based,
    /// the column counted in characters.
    #[arg(long, value_name = "FILE:LINE:COL")]
    explain: Option<String>,

    #[command(flatten)]
    out: output::JsonArgs<ExplainView<'static>>,
}

/// `heph tool build-lsp --explain … --json`: the classification of one
/// position. Fields are declared in key order, which is the order the command
/// printed them in before it opted in.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct ExplainView<'a> {
    /// 1-based column, counted in characters.
    col: u32,
    /// What a present answer could not do in full.
    degraded: &'a [serde_json::Value],
    /// The BUILD file, as given.
    file: &'a str,
    /// 1-based line.
    line: u32,
    /// Why heph's answer there would be empty; `null` when heph answers.
    reason: &'a serde_json::Value,
    /// The site under the cursor: its `kind` and what the resolver read there.
    site: &'a serde_json::Value,
}

impl<'a> From<&'a LspExplanation> for ExplainView<'a> {
    fn from(e: &'a LspExplanation) -> Self {
        Self {
            col: e.col,
            degraded: &e.degraded,
            file: &e.file,
            line: e.line,
            reason: &e.reason,
            site: &e.site,
        }
    }
}

pub fn execute(
    args: &Args,
    _sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    if let Some(target) = &args.explain {
        let explained = crate::pluginbuildfile::lsp_explain(target)?;
        if args.out.enabled() {
            return args
                .out
                .record(&ExplainView::from(&explained), &output::never_cancelled);
        }
        return writeln!(std::io::stdout(), "{}", explained.text())
            .context("writing the classification");
    }
    // Engine construction spawns background tasks (signal/shutdown handlers), so
    // it must run inside a runtime context. The LSP itself is a blocking stdio
    // loop that owns its own lifecycle via the LSP shutdown/exit handshake, so we
    // run it on this thread rather than through the TUI/engine shutdown path.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for build-lsp")?;
    let _guard = runtime.enter();
    let (engine, _shutdown) = bootstrap::new_engine(global, session)?;
    crate::pluginbuildfile::serve_stdio(engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Subcommand as _;

    fn parse(argv: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        crate::commands::Commands::augment_subcommands(clap::Command::new("heph"))
            .try_get_matches_from(argv)
    }

    fn explanation() -> LspExplanation {
        LspExplanation {
            file: "BUILD".to_string(),
            line: 3,
            col: 7,
            site: serde_json::json!({"kind": "target_key", "call": "target"}),
            reason: serde_json::Value::Null,
            degraded: vec![serde_json::json!({"degraded": "ast_absent"})],
        }
    }

    #[test]
    fn output_flags_need_explain() {
        for flag in [&["--json"][..], &["--jq", "."], &["--fields", "site"]] {
            let argv = [&["heph", "tool", "build-lsp"][..], flag].concat();
            let err = parse(&argv).expect_err("server mode has no JSON form");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{argv:?}"
            );
        }
        parse(&[
            "heph",
            "tool",
            "build-lsp",
            "--explain",
            "BUILD:1:1",
            "--json",
        ])
        .expect("--json with --explain");
    }

    /// The keys, their order and their values are what `--explain --json`
    /// printed before the command opted in: `file`, `line` and `col` merged
    /// with the resolver's `site`, `reason` and `degraded`.
    #[test]
    fn explain_json_keeps_its_shape() {
        let e = explanation();
        let mut out = Vec::new();
        output::JsonArgs::<ExplainView<'static>>::try_parse_from(["--json"])
            .expect("--json parses")
            .printer_to(&mut out, output::Style::Compact)
            .record(&ExplainView::from(&e), &output::never_cancelled)
            .expect("print");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "{\"col\":7,\"degraded\":[{\"degraded\":\"ast_absent\"}],\"file\":\"BUILD\",\
             \"line\":3,\"reason\":null,\"site\":{\"call\":\"target\",\"kind\":\"target_key\"}}\n"
        );
    }

    #[test]
    fn explain_text_names_the_site() {
        assert_eq!(
            explanation().text(),
            "BUILD:3:7\n  site: target_key\n  call: \"target\"\n  \
             reason: none (heph answers here)\n  degraded: {\"degraded\":\"ast_absent\"}"
        );
    }

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        output::check_keys(&ExplainView::from(&explanation()))
    }
}
