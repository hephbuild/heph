use anyhow::Context as _;
use std::io::Write as _;

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    out: output::JsonArgs<VersionRecord>,
}

/// `heph version --json`.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct VersionRecord {
    /// The heph version, with its release flavour, as `heph version` prints it.
    version: String,
    /// The version of the JSON output contract. It moves only if a shipped
    /// field of any command is ever renamed, removed or retyped.
    output_contract: u32,
}

impl VersionRecord {
    fn current() -> Self {
        Self {
            version: crate::version::reported(),
            output_contract: output::OUTPUT_CONTRACT,
        }
    }
}

pub fn execute(args: &Args) -> anyhow::Result<()> {
    let record = VersionRecord::current();
    if args.out.enabled() {
        // No request, no Ctrl-C handler: SIGINT ends the process as is.
        return args.out.record(&record, &output::never_cancelled);
    }
    writeln!(std::io::stdout(), "{}", record.version).context("printing the version")
}

#[cfg(test)]
mod tests {
    use super::*;
    use output::Style;

    fn print(flags: &[&str]) -> String {
        let args = output::JsonArgs::<VersionRecord>::try_parse_from(flags).unwrap();
        let mut out = Vec::new();
        args.printer_to(&mut out, Style::Compact)
            .record(&VersionRecord::current(), &output::never_cancelled)
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn version_json() {
        let doc: serde_json::Value = serde_json::from_str(&print(&["--json"])).unwrap();
        assert_eq!(doc["version"], crate::version::reported());
        assert_eq!(doc["output_contract"], 2);
        assert_eq!(doc.as_object().unwrap().len(), 2);
        assert_eq!(print(&["--jq", ".output_contract"]), "2\n");
        assert_eq!(
            print(&["--fields", "version"]),
            format!("{{\"version\":\"{}\"}}\n", crate::version::reported())
        );
    }

    #[test]
    fn record_keys_subset_of_fields() {
        output::check_keys(&VersionRecord::current()).unwrap();
    }
}
