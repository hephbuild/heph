use std::io::Write;

use anyhow::Context;

use crate::engine::config_yaml;

#[derive(clap::Args)]
pub struct Args {}

/// Print the `.hephconfig` JSON Schema to stdout.
///
/// Reads no config and starts no engine: the schema is this binary's, so a
/// workspace that pins a version gets the pinned binary's (the self-upgrade
/// has already re-executed into it by the time this runs).
pub fn execute(_args: &Args) -> anyhow::Result<()> {
    let text = config_yaml::config_json_schema_text().context("rendering the config schema")?;
    write_ignoring_closed_pipe(&mut std::io::stdout().lock(), text.as_bytes())
}

/// Write `bytes` to `out` and flush it. A reader that stopped early
/// (`heph tool config-schema | head -1`) is not a failure: it had what it
/// wanted.
fn write_ignoring_closed_pipe(out: &mut impl Write, bytes: &[u8]) -> anyhow::Result<()> {
    let written = out.write_all(bytes).and_then(|()| out.flush());
    match written {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing the config schema to stdout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_closed_pipe_is_success() {
        write_ignoring_closed_pipe(&mut Failing(std::io::ErrorKind::BrokenPipe), b"{}")
            .expect("a reader that stopped early is not an error");
    }

    #[test]
    fn other_write_errors_fail() {
        let err = write_ignoring_closed_pipe(&mut Failing(std::io::ErrorKind::Other), b"{}")
            .expect_err("a failed write is an error");
        assert!(
            format!("{err:#}").contains("writing the config schema"),
            "{err:#}"
        );
    }

    #[test]
    fn writes_the_bytes() {
        let mut out = Vec::new();
        write_ignoring_closed_pipe(&mut out, b"{}\n").expect("write");
        assert_eq!(out, b"{}\n");
    }
}
