//! A mistyped subcommand's suggestion (error-reporting D4, CLI-10).
//!
//! clap ranks every name a subcommand answers to, its aliases included, so
//! `heph rnu` and even `heph frobnicate` were told "a similar subcommand
//! exists: 'r'". The suggestion is recomputed with the near-miss helper over
//! the level's visible subcommand names only.

use clap::error::{ContextKind, ContextValue, ErrorKind};

/// `err` with its subcommand suggestion recomputed, when it refuses an
/// unknown subcommand: the nearest visible subcommand *names* of the level
/// the word was typed at, never an alias, and none at all when nothing is
/// near ([`hcore::suggest`]). Any other error comes back as it was; the
/// kind, and so the exit code (2), never changes.
///
/// The level is found by following `argv`'s words down `cmd`'s subcommands,
/// the way clap dispatched them, until the refused word.
pub fn with_subcommand_suggestion(
    mut err: clap::Error,
    cmd: &clap::Command,
    argv: impl IntoIterator<Item = std::ffi::OsString>,
) -> clap::Error {
    if err.kind() != ErrorKind::InvalidSubcommand {
        return err;
    }
    let Some(ContextValue::String(invalid)) = err.get(ContextKind::InvalidSubcommand) else {
        return err;
    };
    let invalid = invalid.clone();
    let mut cur = cmd;
    for word in argv.into_iter().skip(1) {
        let Some(word) = word.to_str() else {
            continue;
        };
        if word == invalid {
            break;
        }
        if let Some(sub) = cur
            .get_subcommands()
            .find(|s| s.get_name() == word || s.get_all_aliases().any(|a| a == word))
        {
            cur = sub;
        }
    }
    let names = hcore::suggest::suggest(
        &invalid,
        cur.get_subcommands()
            .filter(|s| !s.is_hide_set())
            .map(clap::Command::get_name),
    );
    if names.is_empty() {
        err.remove(ContextKind::SuggestedSubcommand);
    } else {
        err.insert(
            ContextKind::SuggestedSubcommand,
            ContextValue::Strings(names.into_iter().map(str::to_owned).collect()),
        );
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::gendocs::cli_command;

    fn rendered(argv: &[&str]) -> (clap::Error, String) {
        let cli = cli_command();
        let err = cli
            .clone()
            .try_get_matches_from(argv)
            .expect_err("an unknown subcommand");
        let err = with_subcommand_suggestion(err, &cli, argv.iter().map(Into::into));
        let text = err.render().to_string();
        (err, text)
    }

    /// CLI-10: a typo gets the full name, an unrelated word no tip, and an
    /// alias is never the suggestion, at the top level or below it.
    #[test]
    fn subcommand_typo_never_suggests_alias() {
        let (err, text) = rendered(&["heph", "rnu", "//c:ok"]);
        assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
        assert_eq!(err.exit_code(), 2);
        assert!(
            text.contains("a similar subcommand exists: 'run'"),
            "{text}"
        );
        assert!(!text.contains("'r'"), "{text}");

        let (_, text) = rendered(&["heph", "frobnicate"]);
        assert!(
            text.contains("unrecognized subcommand 'frobnicate'"),
            "{text}"
        );
        assert!(!text.contains("similar subcommand"), "{text}");

        // Below the top level, after a global flag.
        let (_, text) = rendered(&["heph", "--fail-fast", "inspect", "failur", "//c:ok"]);
        assert!(
            text.contains("a similar subcommand exists: 'failure'"),
            "{text}"
        );

        // An alias typed exactly still dispatches: no error to rewrite.
        assert!(
            cli_command()
                .try_get_matches_from(["heph", "r", "//c:ok"])
                .is_ok()
        );
    }
}
