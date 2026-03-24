//! Output formatting utilities

use colored::*;
use std::sync::atomic::{AtomicBool, Ordering};

static NO_COLOR: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Configure output settings
pub fn configure(no_color: bool, verbose: bool) {
    NO_COLOR.store(no_color, Ordering::Relaxed);
    VERBOSE.store(verbose, Ordering::Relaxed);

    if no_color {
        colored::control::set_override(false);
    }
}

/// Check if color output is enabled
pub fn is_color_enabled() -> bool {
    !NO_COLOR.load(Ordering::Relaxed)
}

/// Check if verbose output is enabled
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Print a success message
pub fn success(msg: &str) {
    if is_color_enabled() {
        println!("{} {}", "✓".green().bold(), msg);
    } else {
        println!("✓ {}", msg);
    }
}

/// Print an error message
pub fn error(msg: &str) {
    if is_color_enabled() {
        eprintln!("{} {}", "✗".red().bold(), msg);
    } else {
        eprintln!("✗ {}", msg);
    }
}

/// Print a warning message
pub fn warning(msg: &str) {
    if is_color_enabled() {
        println!("{} {}", "⚠".yellow().bold(), msg);
    } else {
        println!("⚠ {}", msg);
    }
}

/// Print an info message
pub fn info(msg: &str) {
    if is_color_enabled() {
        println!("{} {}", "ℹ".blue().bold(), msg);
    } else {
        println!("ℹ {}", msg);
    }
}

/// Print a verbose message (only if verbose is enabled)
pub fn verbose(msg: &str) {
    if is_verbose() {
        if is_color_enabled() {
            println!("{} {}", "→".cyan(), msg);
        } else {
            println!("→ {}", msg);
        }
    }
}

/// Print a section header
pub fn section(title: &str) {
    if is_color_enabled() {
        println!("\n{}", title.bold().underline());
    } else {
        println!("\n{}", title);
        println!("{}", "=".repeat(title.len()));
    }
}

/// Format a target reference
pub fn format_target(target: &str) -> String {
    if is_color_enabled() {
        target.cyan().to_string()
    } else {
        target.to_string()
    }
}

/// Format a path
pub fn format_path(path: &str) -> String {
    if is_color_enabled() {
        path.yellow().to_string()
    } else {
        path.to_string()
    }
}

/// Format a number
pub fn format_number(num: impl std::fmt::Display) -> String {
    if is_color_enabled() {
        num.to_string().green().bold().to_string()
    } else {
        num.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_configure() {
        configure(true, false);
        assert!(!is_color_enabled());
        assert!(!is_verbose());

        configure(false, true);
        assert!(is_color_enabled());
        assert!(is_verbose());
    }

    #[test]
    fn test_format_target() {
        configure(false, false);
        assert_eq!(format_target("//foo:bar"), "//foo:bar");
    }

    #[test]
    fn test_format_path() {
        configure(false, false);
        assert_eq!(format_path("/tmp/test"), "/tmp/test");
    }

    #[test]
    fn test_format_number() {
        configure(false, false);
        assert_eq!(format_number(42), "42");
    }
}
