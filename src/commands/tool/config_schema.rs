use crate::engine::config_yaml;

#[derive(clap::Args)]
pub struct Args {
    // JSON either way, with no field table: the document is a JSON Schema
    // whose shape follows `ConfigYaml` (D11).
    #[command(flatten)]
    pub out: output::DocArgs,
}

/// Print the `.hephconfig` JSON Schema to stdout: pretty on a terminal, one
/// line when piped.
///
/// Reads no config and starts no engine: the schema is this binary's, so a
/// workspace that pins a version gets the pinned binary's (the self-upgrade
/// has already re-executed into it by the time this runs). A reader that
/// stopped early (`heph tool config-schema | head -1`) is not a failure: it
/// had what it wanted.
pub fn execute(args: &Args) -> anyhow::Result<()> {
    args.out
        .print(&config_yaml::config_json_schema(), &output::never_cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use std::io::Write;

    /// A writer whose reader has gone, or that fails some other way.
    struct Failing(std::io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn print_to(w: &mut impl Write, flags: &[&str]) -> anyhow::Result<()> {
        output::DocArgs::try_parse_from(flags)
            .context("parsing the output flags")?
            .print_to(
                w,
                output::Style::Compact,
                &config_yaml::config_json_schema(),
                &output::never_cancelled,
            )
    }

    #[test]
    fn a_closed_pipe_is_success() {
        print_to(&mut Failing(std::io::ErrorKind::BrokenPipe), &[])
            .expect("a reader that stopped early is not an error");
    }

    #[test]
    fn other_write_errors_fail() {
        let err = print_to(&mut Failing(std::io::ErrorKind::Other), &[])
            .expect_err("a failed write is an error");
        assert!(format!("{err:#}").contains("writing the JSON"), "{err:#}");
    }

    #[test]
    fn piped_it_is_the_schema_on_one_line() {
        let mut out = Vec::new();
        print_to(&mut out, &[]).expect("print");
        let text = String::from_utf8(out).expect("utf-8");
        assert_eq!(text.lines().count(), 1, "{text}");
        let doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(doc, config_yaml::config_json_schema());
    }

    #[test]
    fn jq_filters_the_schema() {
        let mut out = Vec::new();
        print_to(&mut out, &["--jq", ".type"]).expect("print");
        assert_eq!(String::from_utf8(out).expect("utf-8"), "object\n");
    }
}
