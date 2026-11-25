//! `heph tool coreutils` — what the builtin toolbox contains, and what a name
//! resolves to.
//!
//! The builtins shadow the host's utilities on a target's `PATH`, which is the
//! whole point and also the thing that makes a surprising build hard to
//! explain. These subcommands are the answer to "which `cp` actually ran?".

use std::ffi::OsString;
use std::io::Write as _;

#[derive(clap::Args)]
pub struct CoreutilsArgs {
    #[command(subcommand)]
    pub command: CoreutilsCommands,
}

#[derive(clap::Subcommand)]
pub enum CoreutilsCommands {
    /// List the utilities compiled into this binary
    ///
    /// Prints every applet, the toolbox version that reaches a target's cache
    /// key, and where the implementations come from.
    ///
    /// Example: `heph tool coreutils list`
    List(ListArgs),
    /// Explain what a name resolves to
    ///
    /// Says whether heph ships this utility and what the host would supply
    /// instead — the two candidates a target's `PATH` chooses between.
    ///
    /// Example: `heph tool coreutils which cp`
    Which(WhichArgs),
    /// Run one builtin utility directly
    ///
    /// The same code a target gets, without a sandbox — for checking what a
    /// flag does. Its exit code becomes heph's.
    ///
    /// Example: `heph tool coreutils run cp -r src dst`
    Run(RunArgs),
}

#[derive(clap::Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub out: output::JsonArgs<ListView>,
}

#[derive(clap::Args)]
pub struct WhichArgs {
    /// Utility name, e.g. `cp`
    pub name: String,
    #[command(flatten)]
    pub out: output::JsonArgs<WhichView>,
}

/// `heph tool coreutils list --json`.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct ListView {
    /// The toolbox version, folded into every exec target's def hash while
    /// the toolbox is on.
    version: u32,
    /// Where the implementations come from.
    upstream: &'static str,
    /// Every utility compiled into this binary, by name.
    applets: Vec<&'static str>,
}

impl ListView {
    fn current() -> Self {
        Self {
            version: hcoreutils::COREUTILS_VERSION,
            upstream: hcoreutils::UPSTREAM,
            applets: hcoreutils::APPLETS.iter().map(|a| a.name).collect(),
        }
    }
}

/// `heph tool coreutils which --json`: the two candidates a target's `PATH`
/// chooses between.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct WhichView {
    /// The name asked about.
    name: String,
    /// Whether heph ships a builtin of this name.
    builtin: bool,
    /// What the host's `PATH` resolves it to; `null` when it is not there.
    #[serde(serialize_with = "output::ser::opt_path")]
    host: Option<std::path::PathBuf>,
}

#[derive(clap::Args)]
pub struct RunArgs {
    /// Utility name, e.g. `cp`
    pub name: String,
    /// Arguments passed to it verbatim
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

impl CoreutilsArgs {
    pub fn execute(&self) -> anyhow::Result<()> {
        match &self.command {
            CoreutilsCommands::List(args) => list(args),
            CoreutilsCommands::Which(args) => which(args),
            CoreutilsCommands::Run(args) => run(args),
        }
    }
}

/// `heph tool coreutils list | head` closes the pipe under us. That is the
/// reader saying "enough", not an error worth printing — and printing one turns
/// a normal shell idiom into a scary line in a build log.
fn ignore_broken_pipe(res: std::io::Result<()>) -> anyhow::Result<()> {
    match res {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

fn list(args: &ListArgs) -> anyhow::Result<()> {
    if args.out.enabled() {
        return args
            .out
            .record(&ListView::current(), &output::never_cancelled);
    }
    ignore_broken_pipe(list_text())
}

fn list_text() -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "toolbox version {} ({} utilities, from {})",
        hcoreutils::COREUTILS_VERSION,
        hcoreutils::APPLETS.len(),
        hcoreutils::UPSTREAM,
    )?;
    writeln!(out)?;
    // Four columns of names, so the set reads as a set rather than a scroll.
    let names: Vec<&str> = hcoreutils::APPLETS.iter().map(|a| a.name).collect();
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0) + 2;
    for row in names.chunks(4) {
        let mut line = String::new();
        for name in row {
            line.push_str(&format!("{name:<width$}"));
        }
        writeln!(out, "  {}", line.trim_end())?;
    }
    Ok(())
}

fn which(args: &WhichArgs) -> anyhow::Result<()> {
    let builtin = hcoreutils::is_applet(&args.name);
    let host = which::which(&args.name).ok();
    if args.out.enabled() {
        let view = WhichView {
            name: args.name.clone(),
            builtin,
            host,
        };
        return args.out.record(&view, &output::never_cancelled);
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if builtin {
        writeln!(
            out,
            "{}: builtin (toolbox version {})",
            args.name,
            hcoreutils::COREUTILS_VERSION
        )?;
    } else {
        writeln!(out, "{}: not a builtin", args.name)?;
    }
    match &host {
        Some(path) => writeln!(out, "{}: host {}", args.name, path.display())?,
        None => writeln!(out, "{}: not on the host PATH", args.name)?,
    }

    // The part that actually answers the question, and the part a reader
    // cannot work out from the two lines above.
    writeln!(out)?;
    if builtin {
        writeln!(
            out,
            "A target gets the builtin unless it declares a tool of the same name — a\n\
             target's own tools always win — or the exec/bash driver is configured with\n\
             `coreutils: false`."
        )?;
    } else {
        writeln!(
            out,
            "heph does not ship this one, so a target gets it only by declaring it\n\
             as a tool or by finding it on the driver's search path."
        )?;
    }
    Ok(())
}

fn run(args: &RunArgs) -> anyhow::Result<()> {
    let mut argv = Vec::with_capacity(args.args.len() + 1);
    argv.push(OsString::from(&args.name));
    argv.extend(args.args.iter().cloned());

    match hcoreutils::dispatch(&args.name, argv) {
        // The applet's exit code is the point, so it becomes ours. Nothing
        // after this needs to run: `run` takes no lock and owns no state.
        Some(code) => std::process::exit(code),
        None => anyhow::bail!(
            "no builtin utility named '{}' — `heph tool coreutils list` shows the {} that exist",
            args.name,
            hcoreutils::APPLETS.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `--json` prints for `view`, as piped stdout gets it.
    fn json<R>(view: &R) -> serde_json::Value
    where
        R: output::Record<Static = R> + serde::Serialize + 'static,
    {
        let mut out = Vec::new();
        output::JsonArgs::<R>::try_parse_from(["--json"])
            .expect("--json parses")
            .printer_to(&mut out, output::Style::Compact)
            .record(view, &output::never_cancelled)
            .expect("print");
        serde_json::from_slice(&out).expect("one JSON document")
    }

    #[test]
    fn list_json_names_every_applet() {
        // The JSON shape is what an agent reads; keep it pinned.
        let doc = json(&ListView::current());
        let mut keys: Vec<&str> = doc
            .as_object()
            .expect("a record")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["applets", "upstream", "version"]);
        assert_eq!(doc["version"], hcoreutils::COREUTILS_VERSION);
        let applets = doc["applets"].as_array().expect("applets");
        assert_eq!(applets.len(), hcoreutils::APPLETS.len());
        assert!(applets.iter().any(|v| v == "install"));
    }

    #[test]
    fn which_json_names_both_candidates() {
        let doc = json(&WhichView {
            name: "cp".to_string(),
            builtin: true,
            host: Some("/bin/cp".into()),
        });
        assert_eq!(
            doc,
            serde_json::json!({"name": "cp", "builtin": true, "host": "/bin/cp"})
        );
        let doc = json(&WhichView {
            name: "nope".to_string(),
            builtin: false,
            host: None,
        });
        assert_eq!(doc["host"], serde_json::Value::Null);
    }

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        output::check_keys(&ListView::current())?;
        output::check_keys(&WhichView {
            name: "cp".to_string(),
            builtin: true,
            host: Some("/bin/cp".into()),
        })
    }

    #[test]
    fn which_reports_a_missing_builtin_without_erroring() {
        // `which` answers a question; not shipping the utility is an answer,
        // not a failure, or a script that probes it would have to swallow an
        // exit code to read the output.
        which(&WhichArgs {
            name: "definitely-not-a-builtin".to_string(),
            out: output::JsonArgs::default(),
        })
        .unwrap();
    }

    #[test]
    fn run_rejects_an_unknown_utility() {
        let err = run(&RunArgs {
            name: "awk".to_string(),
            args: vec![],
        })
        .unwrap_err();
        assert!(err.to_string().contains("no builtin utility named 'awk'"));
    }
}
