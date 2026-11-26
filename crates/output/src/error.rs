//! The failures that decide their own exit code.

/// A failure of the output layer whose exit code is not the command's.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    /// The `--jq` filter failed while it ran. Exit 2 on every command, so it
    /// never reads as a command's own code (codegen's "findings" is 1).
    /// Results printed before the error stay on stdout.
    #[error("--jq: {0}")]
    JqRuntime(String),
    /// The command was cancelled (Ctrl-C) while the `--jq` filter ran.
    #[error("cancelled while the --jq filter ran")]
    Cancelled,
    /// An output flag the command refuses in a mode clap cannot see (an
    /// argument's value, like `tool build-fmt -`), refused before anything
    /// ran. A usage error: exit 2, as clap's own refusals.
    #[error("{0}")]
    Refused(String),
}

impl OutputError {
    /// The process exit code this failure maps to.
    pub fn exit_code(&self) -> u8 {
        match self {
            OutputError::JqRuntime(_) | OutputError::Refused(_) => 2,
            OutputError::Cancelled => 130,
        }
    }
}

/// The exit code `e` asks for, if an [`OutputError`] is anywhere in its
/// chain. `main` consults this before any command-specific mapping.
pub fn exit_code(e: &anyhow::Error) -> Option<u8> {
    e.chain()
        .find_map(|cause| cause.downcast_ref::<OutputError>())
        .map(OutputError::exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn exit_code_reads_through_context() {
        let e = anyhow::Error::new(OutputError::JqRuntime("boom".into()))
            .context("printing the result");
        assert_eq!(exit_code(&e), Some(2));
        let e: anyhow::Result<()> = Err(OutputError::Cancelled.into());
        assert_eq!(exit_code(&e.context("x").unwrap_err()), Some(130));
        let e = anyhow::Error::new(OutputError::Refused("no".into())).context("x");
        assert_eq!(exit_code(&e), Some(2));
        assert_eq!(exit_code(&anyhow::anyhow!("other")), None);
    }
}
