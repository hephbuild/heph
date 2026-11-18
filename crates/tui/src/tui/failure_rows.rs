//! The rows a failure ends with — where its log is, what became of its
//! sandbox, what it kept and wrote back, and how to read its log — rendered
//! once for both places that print them: the CI end-of-run box
//! (`src/commands/errors.rs`) and the line backend's live failure block
//! ([`CiProgressView`](crate::tui::CiProgressView)), so the two cannot word
//! them differently.

use hcore::events::{FixesData, LeftBehindData};

/// How many written-back paths the rows name before they point at
/// `fixes.txt`.
pub const FIXES_SHOWN: usize = 10;

/// `s` as one shell word, so a printed command pastes as-is. A path that
/// cannot be quoted (a NUL byte) is printed raw: it could not be passed to a
/// command either.
pub fn shell_quote(s: &str) -> std::borrow::Cow<'_, str> {
    shlex::try_quote(s).unwrap_or(std::borrow::Cow::Borrowed(s))
}

/// The rows, each indented two spaces and ending in a newline, in the box's
/// order: `full log:`, an out-of-the-ordinary `sandbox:`, `outputs:`, the
/// write-back, then `log:` last — the command that reads the log without its
/// path, which an agent can run as-is (two-mode TUI I8), whenever the engine
/// can open one; "none kept" when it cannot. A failure nobody asked the
/// engine about points at `heph log` only beside `full log:`.
pub fn left_behind_rows(addr: &str, left: &LeftBehindData) -> String {
    let mut out = String::new();
    if let Some(path) = &left.log_path {
        out.push_str(&format!("  full log: {path}\n"));
    }
    if let Some(sandbox) = &left.sandbox {
        out.push_str(&format!("  sandbox: {sandbox}\n"));
    }
    if let Some(outputs) = &left.outputs {
        out.push_str(&format!("  outputs: {outputs}\n"));
    }
    if let Some(fixes) = &left.fixes {
        fixes_rows(&mut out, fixes);
    }
    if left.log_readable.unwrap_or(left.log_path.is_some()) {
        out.push_str(&format!("  log: heph log {}\n", shell_quote(addr)));
    } else if left.log_readable == Some(false) {
        out.push_str("  log: none kept\n");
    }
    out
}

/// A failed `in_place` run's write-back (D20): the files it changed and how
/// to undo that, or why it changed nothing. A write-back that matched the
/// tree already says nothing.
///
/// The restore hint runs git in the root the fixes were written under (`-C`),
/// since the paths are workspace-relative and the user's cwd may be anywhere,
/// and with `--literal-pathspecs`, since git reads a pathspec as a glob:
/// `web/[id].ts` would also revert `web/i.ts`. It restores the index version,
/// so it says that uncommitted edits to those files go with it.
fn fixes_rows(out: &mut String, fixes: &FixesData) {
    const RESTORE: &str =
        "  restore with (takes the index version; uncommitted edits to these files are lost):\n";
    match fixes {
        FixesData::Written { paths, .. } if paths.is_empty() => {}
        FixesData::Written { root, paths, list } => {
            let git = format!("git -C {} --literal-pathspecs checkout", shell_quote(root));
            out.push_str(
                "  fixes written, target still failed (re-run to verify; \
                 codegen = \"in_place_on_success\" opts out):\n",
            );
            for p in paths.iter().take(FIXES_SHOWN) {
                out.push_str(&format!("    {p}\n"));
            }
            let more = paths.len().saturating_sub(FIXES_SHOWN);
            match list {
                Some(list) if more > 0 => {
                    out.push_str(&format!("    +{more} more (full list: {list})\n"));
                    // The list is one path per line, which is what git's
                    // `--pathspec-from-file` reads: no xargs word splitting.
                    out.push_str(RESTORE);
                    out.push_str(&format!(
                        "    {git} --pathspec-from-file={}\n",
                        shell_quote(list)
                    ));
                }
                _ => {
                    if more > 0 {
                        out.push_str(&format!("    +{more} more\n"));
                    }
                    // With no list to point at, the hint names every path:
                    // a truncated restore would silently leave files behind.
                    let quoted: Vec<_> = paths.iter().map(|p| shell_quote(p)).collect();
                    out.push_str(RESTORE);
                    out.push_str(&format!("    {git} -- {}\n", quoted.join(" ")));
                }
            }
        }
        FixesData::NotWritten { reason } => {
            out.push_str(&format!("  fixes not written: {reason}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row, in the box's order and words.
    #[test]
    fn rows_come_in_the_box_order() {
        let left = LeftBehindData {
            log_path: Some("/h/failures/p/log.txt".into()),
            sandbox: Some("/h/sandbox/x (kept)".into()),
            outputs: Some("/h/failures/p/out".into()),
            fixes: Some(FixesData::NotWritten {
                reason: "--frozen checks the tree, it never writes it".into(),
            }),
            log_readable: Some(true),
        };
        assert_eq!(
            left_behind_rows("//p:x", &left),
            "  full log: /h/failures/p/log.txt\n\
             \x20 sandbox: /h/sandbox/x (kept)\n\
             \x20 outputs: /h/failures/p/out\n\
             \x20 fixes not written: --frozen checks the tree, it never writes it\n\
             \x20 log: heph log //p:x\n"
        );
    }

    /// The engine's answer decides `log:`; without one, the path does.
    #[test]
    fn log_row_follows_the_engine_then_the_path() {
        let path = Some("/h/log.txt".to_owned());
        let rows = |log_path: Option<String>, log_readable| {
            left_behind_rows(
                "//p:a b",
                &LeftBehindData {
                    log_path,
                    log_readable,
                    ..LeftBehindData::default()
                },
            )
        };
        assert_eq!(rows(None, Some(true)), "  log: heph log '//p:a b'\n");
        assert_eq!(rows(None, Some(false)), "  log: none kept\n");
        assert_eq!(rows(None, None), "");
        assert_eq!(
            rows(path.clone(), None),
            "  full log: /h/log.txt\n  log: heph log '//p:a b'\n"
        );
        assert_eq!(
            rows(path, Some(false)),
            "  full log: /h/log.txt\n  log: none kept\n"
        );
    }
}
