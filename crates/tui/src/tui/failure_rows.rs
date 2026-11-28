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
    if log_readable(left) {
        out.push_str(&format!("  log: heph log {}\n", shell_quote(addr)));
    } else if left.log_readable == Some(false) {
        out.push_str("  log: none kept\n");
    }
    out
}

/// Whether `heph log` can read the log `left` describes: the engine's answer
/// when it gave one, else whether a log was kept on disk.
pub fn log_readable(left: &LeftBehindData) -> bool {
    left.log_readable.unwrap_or(left.log_path.is_some())
}

/// What a failure box prints of a log tail (error-reporting D6.1, D6.1b),
/// decided here once for the three boxes (the report after the TUI, the line
/// backend's live block, the CI end-of-run box), so they cannot word it, or
/// count it, differently.
#[derive(Debug, PartialEq, Eq)]
pub struct TailRows<'a> {
    /// The row printed before the tail's lines, or in their place: the
    /// pointer of a tail that streamed to a terminal, `(no output)`, or
    /// `… N earlier lines` when the log has lines the tail does not show.
    pub note: Option<String>,
    /// The tail's lines to show, a suffix of the tail as given (neutralize
    /// it with [`escape_log_tail`](crate::tui::text::escape_log_tail) before
    /// printing). Empty when `note` stands in for the tail.
    pub shown: &'a str,
    /// The 1-based number, in the whole log, of `shown`'s first line.
    pub first_line: usize,
}

/// The number of lines in the log whose tail is `text`, starting at its line
/// `start_line`: the lines before the tail plus the tail's own. A log carries
/// no total; this is the one place it is worked out.
pub fn log_line_count(text: &str, start_line: usize) -> usize {
    start_line.saturating_sub(1) + text.lines().count()
}

/// What a box shows of the tail `text` of `addr`'s log, whose first line is
/// line `start_line` of the log, showing at most `max_lines` of it.
///
/// The log has N lines: those before the tail (`start_line - 1`) plus the
/// tail's own (no total is carried; the tail's start says the rest).
/// - N = 0: `(no output)`, streamed or not.
/// - `streamed` (its output reached a terminal while it ran, so the user saw
///   it there): the pointer `(output not shown here: N lines · heph log //t)`
///   and no line of the tail (U5). The command only when `readable` (`heph
///   log` can open the log): a pointer must not name a command with nothing
///   to show.
/// - Otherwise the tail's last `max_lines` lines, after an
///   `… N earlier lines` row when the log has lines before them. The row
///   names no command: the box's own `log:` / `full log:` row does, and
///   says when there is none.
pub fn tail_rows<'a>(
    addr: &str,
    text: &'a str,
    start_line: usize,
    streamed: bool,
    readable: bool,
    max_lines: usize,
) -> TailRows<'a> {
    let first = start_line.max(1);
    let before = first - 1;
    let in_tail = text.lines().count();
    let total = log_line_count(text, start_line);
    // `1 line`, `1,500 lines`, `3 earlier lines`.
    let lines = |n: usize, noun: &str| {
        format!(
            "{} {noun}{}",
            hcore::units::thousands(n),
            if n == 1 { "" } else { "s" }
        )
    };
    if total == 0 {
        return TailRows {
            note: Some("(no output)".to_owned()),
            shown: "",
            first_line: first,
        };
    }
    if streamed {
        let note = if readable {
            format!(
                "(output not shown here: {} · heph log {})",
                lines(total, "line"),
                shell_quote(addr)
            )
        } else {
            format!("(output not shown here: {})", lines(total, "line"))
        };
        return TailRows {
            note: Some(note),
            shown: "",
            first_line: first,
        };
    }
    let skip = in_tail.saturating_sub(max_lines);
    let shown = match skip.checked_sub(1) {
        None => text,
        Some(k) => text
            .match_indices('\n')
            .nth(k)
            .and_then(|(i, _)| text.get(i + 1..))
            .unwrap_or(""),
    };
    let earlier = before + skip;
    TailRows {
        note: (earlier > 0).then(|| format!("… {}", lines(earlier, "earlier line"))),
        shown,
        first_line: first + skip,
    }
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

    /// D6.1's tail rows, each case: the pointer of a streamed tail, with N
    /// counted from the tail's start and its own lines; `(no output)` at
    /// N = 0, streamed or not; the earlier-lines row for a tail that starts
    /// past line 1 and for lines `max_lines` leaves out; none for a whole
    /// log; the address quoted as a shell word; no command in a pointer to a
    /// log `heph log` cannot open.
    #[test]
    fn tail_rows_pointer_no_output_and_earlier_lines() {
        let rows =
            |text, start, streamed, max| tail_rows("//p:t", text, start, streamed, true, max);

        assert_eq!(
            tail_rows("//p:t", "a\nb\n", 999, true, false, 1000)
                .note
                .as_deref(),
            Some("(output not shown here: 1,000 lines)"),
            "no log to read: the pointer names no command"
        );
        let r = tail_rows("//p:t", "a\nb\n", 41, false, false, 1000);
        assert_eq!(r.note.as_deref(), Some("… 40 earlier lines"));
        assert_eq!(r.shown, "a\nb\n", "an unreadable log's tail is still shown");
        assert_eq!(
            rows("a", 0, false, 0).note.as_deref(),
            Some("… 1 earlier line"),
            "no line shown, start 0 read as 1"
        );

        let r = rows("a\nb\n", 999, true, 1000);
        assert_eq!(
            r.note.as_deref(),
            Some("(output not shown here: 1,000 lines · heph log //p:t)")
        );
        assert_eq!(r.shown, "", "a streamed tail shows no line of it");
        assert_eq!(
            rows("a", 1, true, 1000).note.as_deref(),
            Some("(output not shown here: 1 line · heph log //p:t)")
        );

        for streamed in [false, true] {
            let r = rows("", 1, streamed, 1000);
            assert_eq!(
                r.note.as_deref(),
                Some("(no output)"),
                "streamed {streamed}"
            );
            assert_eq!(r.shown, "");
        }

        let r = rows("x\ny\n", 41, false, 1000);
        assert_eq!(r.note.as_deref(), Some("… 40 earlier lines"));
        assert_eq!((r.shown, r.first_line), ("x\ny\n", 41));

        let r = rows("a\nb\nc\n", 2, false, 1);
        assert_eq!(r.note.as_deref(), Some("… 3 earlier lines"));
        assert_eq!((r.shown, r.first_line), ("c\n", 4));

        let r = rows("a\nb", 2, false, 1000);
        assert_eq!(r.note.as_deref(), Some("… 1 earlier line"));

        let r = rows("a\nb\n", 1, false, 1000);
        assert_eq!(r.note, None, "the whole log is shown");
        assert_eq!((r.shown, r.first_line), ("a\nb\n", 1));

        assert_eq!(
            tail_rows("//p:a b", "a", 1, true, true, 1000)
                .note
                .as_deref(),
            Some("(output not shown here: 1 line · heph log '//p:a b')")
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
