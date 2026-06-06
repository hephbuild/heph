use std::io::{self, Write};

use anyhow::Context;
use clap_complete::env::{Bash, Elvish, EnvCompleter, Fish, Powershell, Zsh};

#[derive(clap::Args)]
pub struct Args {
    /// Shell to emit the completion-registration script for
    #[arg(value_enum)]
    pub shell: Shell,
}

/// Shells supported by the dynamic completion engine.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

/// Print the registration snippet that wires up dynamic completion for the
/// chosen shell. The script drives completion back through this binary via the
/// `COMPLETE` env var, so target addresses are resolved live (see
/// [`crate::commands::completion`]).
///
/// Install by sourcing it, e.g. for zsh: `source <(heph tool completions zsh)`
/// in `~/.zshrc`.
pub fn execute(args: &Args) -> anyhow::Result<()> {
    let completer: &dyn EnvCompleter = match args.shell {
        Shell::Bash => &Bash,
        Shell::Zsh => &Zsh,
        Shell::Fish => &Fish,
        Shell::Elvish => &Elvish,
        Shell::Powershell => &Powershell,
    };

    let stdout = io::stdout();
    let mut lock = stdout.lock();
    completer
        .write_registration("COMPLETE", "heph", "heph", "heph", &mut lock)
        .context("write completion registration")?;
    lock.flush().context("flush stdout")?;
    Ok(())
}
