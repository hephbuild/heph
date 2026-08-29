use clap::{Args, Subcommand};

use crate::commands::{Commands, GlobalOptions};

const BIN: &str = "heph";

#[derive(clap::Args)]
pub struct GenDocsArgs {}

pub fn execute(_args: &GenDocsArgs) -> anyhow::Result<()> {
    print!("{}", render_markdown(&cli_command()));

    Ok(())
}

/// Rebuild the full clap command tree from the derive-generated augmenters so the
/// generator (and its tests) can introspect the live CLI without depending on the
/// `Cli` struct that lives in the binary crate's `main.rs`.
pub fn cli_command() -> clap::Command {
    let cmd = clap::Command::new(BIN).about("An efficient build system");
    let cmd = GlobalOptions::augment_args(cmd);
    Commands::augment_subcommands(cmd)
}

/// Render a markdown reference for every visible subcommand of `cmd`.
pub fn render_markdown(cmd: &clap::Command) -> String {
    let mut out = String::new();
    for sub in cmd.get_subcommands() {
        render_command(sub, BIN, &mut out);
    }
    out
}

fn render_command(cmd: &clap::Command, parent_path: &str, out: &mut String) {
    if cmd.is_hide_set() {
        return;
    }

    let path = format!("{parent_path} {}", cmd.get_name());

    out.push_str(&format!("## `{path}`\n\n"));

    // Prefer the long description (multi-paragraph doc comment incl. examples)
    // so the reference carries the same detail as `--help`; fall back to the
    // one-line about for commands without one.
    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        out.push_str(&format!("{about}\n\n"));
    }

    // clap's auto usage starts with the command's own leaf name, so prefix the
    // parent path to read `heph <path> <args…>`. An explicit override_usage already
    // spells out the full `heph …` invocation, so it must NOT be re-prefixed — detect
    // that by the line already leading with the bin name. Continuation lines are
    // indented to align under clap's `Usage: ` column in `--help`; strip that
    // alignment whitespace for clean markdown.
    let bin_prefix = format!("{BIN} ");
    let usage = cmd.clone().render_usage().to_string();
    let usage = usage.trim_start_matches("Usage:").trim();
    let usage = usage
        .lines()
        .map(|line| {
            let line = line.trim_start();
            if line == BIN || line.starts_with(&bin_prefix) {
                line.to_string()
            } else {
                format!("{parent_path} {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str(&format!("```bash\n{usage}\n```\n\n"));

    if let Some(table) = flags_table(cmd) {
        out.push_str(&table);
        out.push('\n');
    }

    // The after-help block (e.g. `run`/`query`'s query-language reference) is
    // help-only in clap; surface it in the generated docs as a fenced block so
    // the markdown reference is as complete as `--help`.
    if let Some(after) = cmd.get_after_long_help().or_else(|| cmd.get_after_help()) {
        out.push_str(&format!("```text\n{after}\n```\n\n"));
    }

    for child in cmd.get_subcommands() {
        render_command(child, &path, out);
    }
}

/// Build a markdown table of a command's documentable arguments, or `None` when it
/// has none (so the caller can skip emitting an empty table).
fn flags_table(cmd: &clap::Command) -> Option<String> {
    let mut rows = String::new();
    let mut any = false;

    for arg in cmd.get_arguments() {
        if arg.is_hide_set() || arg.get_id() == "help" || arg.get_id() == "version" {
            continue;
        }
        any = true;

        let flag = if let Some(long) = arg.get_long() {
            match arg.get_short() {
                Some(short) => format!("`-{short}`, `--{long}`"),
                None => format!("`--{long}`"),
            }
        } else if let Some(short) = arg.get_short() {
            format!("`-{short}`")
        } else {
            // positional
            format!("`<{}>`", arg.get_id().as_str().to_uppercase())
        };

        // Boolean flags (SetTrue/Count/…) carry a value-name placeholder but take no
        // value — leave their Value column blank.
        let takes_value = arg.get_action().takes_values();
        let value = if takes_value {
            arg.get_value_names()
                .map(|names| {
                    names
                        .iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default()
        } else {
            String::new()
        };

        let default = arg
            .get_default_values()
            .iter()
            .map(|v| format!("`{}`", v.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");

        let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();

        rows.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            escape(&flag),
            escape(&value),
            escape(&default),
            escape(&help),
        ));
    }

    if !any {
        return None;
    }

    let mut table = String::from("| Flag | Value | Default | Description |\n");
    table.push_str("| --- | --- | --- | --- |\n");
    table.push_str(&rows);
    Some(table)
}

fn escape(s: &str) -> String {
    s.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_real_cli_tree() {
        let md = render_markdown(&cli_command());

        // Top-level commands appear.
        assert!(md.contains("## `heph run`"), "missing run heading:\n{md}");
        assert!(md.contains("## `heph query`"), "missing query heading");

        // Nested inspect subcommands flatten with the full path.
        assert!(
            md.contains("## `heph inspect packages`"),
            "missing nested inspect packages heading:\n{md}"
        );

        // Flags from the real command structs land in the tables.
        assert!(md.contains("`--force`"), "missing --force flag row");

        // The query-language flag and after-help reference are surfaced in docs.
        assert!(
            md.contains("`-e`, `--expr`"),
            "missing expr flag row:\n{md}"
        );
        assert!(
            md.contains("Query language (-e / --expr):"),
            "query language after-help reference missing from docs:\n{md}"
        );
    }

    /// Split a documented command line into argv, honouring the single quotes the
    /// examples use around query expressions (`-e '//... && !//vendor/...'`).
    fn argv(example: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut started = false;
        let mut quoted = false;
        for c in example.chars() {
            match c {
                '\'' => {
                    quoted = !quoted;
                    started = true;
                }
                c if c.is_whitespace() && !quoted => {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            }
        }
        if started {
            out.push(cur);
        }
        out
    }

    /// Every `heph …` command line documented in `text`, from both the backticked
    /// form the doc comments use and the bare form of the after-help reference
    /// block. Lines carrying a `<PLACEHOLDER>` describe a form rather than name a
    /// runnable command, so they are left out.
    fn examples_in(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in text.lines() {
            let mut found: Vec<&str> = line
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|span| span.starts_with("heph "))
                .collect();
            // Bare `heph …` line in the after-help block, with prose aligned two
            // or more spaces to its right.
            let bare = line.trim_start();
            if found.is_empty()
                && bare.starts_with("heph ")
                && let Some(cmd) = bare.split("  ").next()
            {
                found.push(cmd);
            }
            out.extend(
                found
                    .into_iter()
                    .map(str::trim)
                    .filter(|e| !e.contains('<'))
                    .map(str::to_string),
            );
        }
        out
    }

    fn collect_examples(cmd: &clap::Command, out: &mut Vec<String>) {
        for text in [
            cmd.get_long_about().or_else(|| cmd.get_about()),
            cmd.get_after_long_help().or_else(|| cmd.get_after_help()),
        ]
        .into_iter()
        .flatten()
        {
            out.extend(examples_in(&text.to_string()));
        }
        for child in cmd.get_subcommands() {
            collect_examples(child, out);
        }
    }

    /// Every command line the help advertises must be one the CLI accepts.
    ///
    /// `heph query //...` shipped in the `query` help while the single-positional
    /// form parses its argument as an *address*, so the advertised command could
    /// not run at all — the whole-workspace selection needs `-e '//...'`. Copying
    /// an example out of `--help` is the first thing anyone does, so an example
    /// that does not parse is a bug in the CLI, not just in its prose.
    #[test]
    fn documented_examples_are_accepted() {
        use crate::commands::utils::resolve_matcher;
        use crate::htpkg::PkgBuf;

        let root = cli_command();
        let mut examples = Vec::new();
        collect_examples(&root, &mut examples);
        assert!(
            examples.len() > 20,
            "example extraction found almost nothing ({examples:?}) — the scan is broken, \
             not the help"
        );

        for example in &examples {
            let matches = root
                .clone()
                .try_get_matches_from(argv(example))
                .unwrap_or_else(|e| panic!("`{example}` is not accepted by the CLI:\n{e}"));

            // Walk to the leaf subcommand; the selection args only exist there.
            let mut leaf = (String::from("heph"), &matches);
            while let Some((name, sub)) = leaf.1.subcommand() {
                leaf = (format!("{} {name}", leaf.0), sub);
            }
            let (path, m) = leaf;

            // `run`/`query`/`tool clean` share the selection form, and clap
            // accepting the string says nothing about whether it resolves.
            let ids: Vec<_> = m.ids().map(|i| i.as_str()).collect();
            if !ids.contains(&"arg1") && !ids.contains(&"expr") {
                continue;
            }
            let get = |id| m.try_get_one::<String>(id).ok().flatten().cloned();
            // `run` is the one selection that rejects the `all` label.
            let allow_all = path != "heph run";
            resolve_matcher(
                &get("expr"),
                &get("arg1"),
                &get("arg2"),
                &PkgBuf::from(""),
                allow_all,
            )
            .unwrap_or_else(|e| panic!("`{example}` parses but selects nothing: {e:#}"));
        }
    }

    #[test]
    fn hidden_command_is_omitted() {
        let md = render_markdown(&cli_command());
        assert!(
            !md.contains("gen-docs"),
            "hidden gen-docs leaked into output:\n{md}"
        );
    }

    #[test]
    fn multiline_override_usage_not_reprefixed_and_dedented() {
        let mut out = String::new();
        // override_usage already spells the full `heph query` invocation; its
        // continuation line aligns under clap's `Usage: ` column in --help.
        let cmd = clap::Command::new("query").override_usage(
            "heph query <TARGET_ADDRESS>\n       heph query <LABEL> <PACKAGE_MATCHER>",
        );
        render_command(&cmd, BIN, &mut out);

        // No double `heph heph` prefix; alignment whitespace stripped in markdown.
        assert!(
            out.contains("heph query <TARGET_ADDRESS>\nheph query <LABEL> <PACKAGE_MATCHER>"),
            "override usage mangled:\n{out}"
        );
        assert!(
            !out.contains("heph heph"),
            "override usage was double-prefixed:\n{out}"
        );
        assert!(
            !out.contains("       heph"),
            "stale alignment whitespace leaked:\n{out}"
        );
    }

    #[test]
    fn auto_usage_gets_parent_prefix() {
        let mut out = String::new();
        let cmd = clap::Command::new("run").arg(clap::Arg::new("addr"));
        render_command(&cmd, BIN, &mut out);

        assert!(
            out.contains("heph run"),
            "auto usage missing parent prefix:\n{out}"
        );
    }

    #[test]
    fn flags_table_builds_row() {
        let cmd = clap::Command::new("demo").arg(
            clap::Arg::new("force")
                .long("force")
                .short('f')
                .default_value("off")
                .help("Force it"),
        );

        let table = flags_table(&cmd).expect("table for command with args");
        assert!(table.contains("| Flag | Value | Default | Description |"));
        assert!(
            table.contains("| `-f`, `--force` |"),
            "row missing composed flag: {table}"
        );
        assert!(table.contains("`off`"), "row missing default: {table}");
        assert!(table.contains("Force it"), "row missing help: {table}");
    }

    #[test]
    fn long_about_preferred_over_short() {
        let mut out = String::new();
        let cmd = clap::Command::new("run")
            .about("short")
            .long_about("long body\n\nExamples:\n  heph run //pkg:bin");
        render_command(&cmd, BIN, &mut out);

        assert!(
            out.contains("long body") && out.contains("heph run //pkg:bin"),
            "long_about (with examples) not rendered:\n{out}"
        );
        assert!(
            !out.contains("short\n"),
            "short about leaked when long_about present:\n{out}"
        );
    }

    #[test]
    fn falls_back_to_short_about_when_no_long() {
        let mut out = String::new();
        let cmd = clap::Command::new("ver").about("Prints version");
        render_command(&cmd, BIN, &mut out);

        assert!(
            out.contains("Prints version"),
            "short about not rendered as fallback:\n{out}"
        );
    }

    #[test]
    fn real_cli_examples_reach_markdown() {
        let md = render_markdown(&cli_command());
        assert!(
            md.contains("heph run //cmd/server:bin"),
            "run command examples missing from reference:\n{md}"
        );
    }

    #[test]
    fn flags_table_none_when_no_args() {
        let cmd = clap::Command::new("bare");
        assert!(flags_table(&cmd).is_none());
    }
}
