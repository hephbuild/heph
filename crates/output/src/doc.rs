//! The output flags of a command that always prints JSON, with no field
//! table: `--json` and `--jq`, no `--fields` (D11).

use crate::Cancelled;
use crate::jq::Jq;
use crate::write::{Style, value_document};
use serde::Serialize;
use std::io::Write;

/// `--json` and `--jq` for a command whose output is a JSON document either
/// way, and whose shape is not a contract: `inspect spec`, `inspect def`,
/// `tool config-schema`. It has no field table, so there is no `--fields`
/// and nothing for the I3 snapshot to pin.
///
/// The flags form the same clap group as [`crate::JsonArgs`], `output`.
#[derive(clap::Args, Clone, Debug, Default)]
#[group(id = "output", multiple = true)]
pub struct DocArgs {
    /// Print the result as JSON on stdout, and nothing else there
    ///
    /// The command prints JSON either way; this also keeps the progress
    /// display off. Pretty on a terminal, one line when piped.
    #[arg(long, help_heading = "Output")]
    json: bool,

    /// Filter the JSON through a jq expression (implies --json)
    ///
    /// Each result is printed on its own line: a string raw, anything else
    /// as compact JSON. A filter that fails while it runs exits 2.
    #[arg(long, value_name = "EXPR", value_parser = Jq::compile, help_heading = "Output")]
    jq: Option<Jq>,
}

impl DocArgs {
    /// Parse the output flags alone. For tests.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        use clap::{Args as _, FromArgMatches as _};
        let cmd = Self::augment_args(clap::Command::new("heph").no_binary_name(true));
        let matches = cmd.try_get_matches_from(args)?;
        Self::from_arg_matches(&matches)
    }

    /// Whether `--json` or `--jq` was given.
    pub fn enabled(&self) -> bool {
        self.json || self.jq.is_some()
    }

    /// Print `value` on stdout: the whole document, or the `--jq` results.
    /// Printed whether or not a flag was given.
    pub fn print<T: Serialize + ?Sized>(
        &self,
        value: &T,
        cancelled: Cancelled<'_>,
    ) -> anyhow::Result<()> {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        self.print_to(&mut out, Style::for_stdout(), value, cancelled)
    }

    /// [`DocArgs::print`] on `w`, laid out as `style`. For tests.
    pub fn print_to<T: Serialize + ?Sized>(
        &self,
        w: &mut impl Write,
        style: Style,
        value: &T,
        cancelled: Cancelled<'_>,
    ) -> anyhow::Result<()> {
        value_document(w, value, style, self.jq.as_ref(), cancelled)
    }
}

/// What a command runner needs to know of a command's output flags: whether
/// one was given, so the TUI stays off (I1). Implemented by
/// [`crate::JsonArgs`] and [`DocArgs`].
pub trait OutputFlags {
    /// Whether any output flag was given.
    fn enabled(&self) -> bool;
}

impl<R: crate::Record + 'static> OutputFlags for crate::JsonArgs<R> {
    fn enabled(&self) -> bool {
        crate::JsonArgs::enabled(self)
    }
}

impl OutputFlags for DocArgs {
    fn enabled(&self) -> bool {
        DocArgs::enabled(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn print(flags: &[&str], style: Style) -> String {
        let args = DocArgs::try_parse_from(flags).expect("flags parse");
        let mut out = Vec::new();
        args.print_to(
            &mut out,
            style,
            &serde_json::json!({"a": [1, 2], "b": "x"}),
            &crate::never_cancelled,
        )
        .expect("print");
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn doc_args_print_json_with_or_without_a_flag() {
        assert_eq!(print(&[], Style::Compact), "{\"a\":[1,2],\"b\":\"x\"}\n");
        assert_eq!(
            print(&["--json"], Style::Compact),
            print(&[], Style::Compact)
        );
        assert!(print(&[], Style::Pretty).contains("\n  \"a\": [\n"));
        assert_eq!(print(&["--jq", ".b"], Style::Pretty), "x\n");
        assert_eq!(print(&["--jq", ".a[]"], Style::Compact), "1\n2\n");
    }

    #[test]
    fn doc_args_have_no_fields_flag() {
        DocArgs::try_parse_from(["--fields", "a"]).unwrap_err();
        DocArgs::try_parse_from(["--jq", "env"]).unwrap_err();
        assert!(
            !DocArgs::try_parse_from(Vec::<&str>::new())
                .unwrap()
                .enabled()
        );
        assert!(DocArgs::try_parse_from(["--jq", "."]).unwrap().enabled());
    }
}
