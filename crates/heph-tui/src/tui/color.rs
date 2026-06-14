//! Shared ANSI color capability detection for stderr.
//!
//! All human-facing output (logs, error rendering, the TUI) goes to stderr, so
//! a single predicate governs whether we emit ANSI escapes: only when stderr is
//! a terminal and `NO_COLOR` (https://no-color.org) is unset. Redirecting stderr
//! to a file or pipe disables color, keeping logs free of escape sequences.

use std::io::IsTerminal;

/// Whether ANSI color should be emitted on stderr.
pub fn stderr_color_enabled() -> bool {
    color_enabled(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// Pure decision: color only when the stream is a terminal and `NO_COLOR` is
/// unset. Split out so the policy is testable without a real tty.
fn color_enabled(is_terminal: bool, no_color_set: bool) -> bool {
    is_terminal && !no_color_set
}

#[cfg(test)]
mod tests {
    use super::color_enabled;

    #[test]
    fn color_only_on_terminal_without_no_color() {
        assert!(color_enabled(true, false));
        assert!(
            !color_enabled(true, true),
            "NO_COLOR set must disable color"
        );
        assert!(!color_enabled(false, false), "non-tty must disable color");
        assert!(!color_enabled(false, true));
    }
}
