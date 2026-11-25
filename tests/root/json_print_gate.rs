//! I1, structurally: a command module that opts into the output flags
//! (`output::JsonArgs`, `output::DocArgs`) never uses `print!`/`println!`.
//!
//! Under `--json` stdout carries the document and nothing else. A command
//! writes its human output with `writeln!(stdout)` behind
//! `!out.enabled()`, or through the TUI's buffered stdout, both of which
//! return the write error a `println!` turns into a panic on a closed pipe.
//! A `println!` slipped into such a module is the one line that can land in
//! the middle of a document, and no test of the JSON path would notice it
//! unless that exact branch ran.
//!
//! Cheap: a walk of `src/commands` and some string work, no build.

use super::execrunner_gate::{repo_root, rust_files};

/// What makes a module opted in.
const OPT_IN: &[&str] = &["output::JsonArgs<", "output::DocArgs"];

/// The macros that write to stdout and panic when it is closed.
const PRINTS: &[&str] = &["println!(", "print!("];

#[test]
fn no_print_in_opted_in_commands() {
    let mut files = Vec::new();
    rust_files(&repo_root().join("src/commands"), &mut files);
    let mut opted_in = 0;
    let mut offenders = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read source");
        // Comments may name the macros; code may not.
        let code: Vec<(usize, &str)> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim_start().starts_with("//"))
            .collect();
        if !code
            .iter()
            .any(|(_, l)| OPT_IN.iter().any(|p| l.contains(p)))
        {
            continue;
        }
        opted_in += 1;
        for (i, line) in code {
            // `eprintln!`/`eprint!` write to stderr, which is allowed.
            let prints = PRINTS.iter().any(|p| {
                line.match_indices(p)
                    .any(|(at, _)| !line.get(..at).is_some_and(|s| s.ends_with('e')))
            });
            if prints {
                let rel = path.strip_prefix(repo_root()).unwrap_or(&path);
                offenders.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        opted_in > 30,
        "found only {opted_in} opted-in modules: the opt-in spelling changed and this \
         gate is scanning almost nothing"
    );
    assert!(
        offenders.is_empty(),
        "print!/println! in a module that opts into --json (I1); write with \
         `writeln!(std::io::stdout(), …)` behind `!out.enabled()` instead:\n{}",
        offenders.join("\n")
    );
}
