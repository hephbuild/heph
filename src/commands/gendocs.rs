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

    if let Some(about) = cmd.get_about() {
        out.push_str(&format!("{about}\n\n"));
    }

    // clap's usage starts with the command's own leaf name; prefix the parent path
    // so it reads `rheph <path> <args…>`. Multi-line override_usage forms pass
    // through verbatim, matching how `--help` renders them.
    let usage = cmd
        .clone()
        .render_usage()
        .to_string()
        .trim_start_matches("Usage:")
        .trim()
        .to_string();
    out.push_str(&format!("```bash\n{parent_path} {usage}\n```\n\n"));

    if let Some(table) = flags_table(cmd) {
        out.push_str(&table);
        out.push('\n');
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
        assert!(
            md.contains("`-e`, `--exclude`"),
            "missing short+long exclude flag row"
        );
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
    fn flags_table_none_when_no_args() {
        let cmd = clap::Command::new("bare");
        assert!(flags_table(&cmd).is_none());
    }
}
