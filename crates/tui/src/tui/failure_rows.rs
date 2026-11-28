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
    left_behind_rows_in(addr, left, true, true)
}

/// [`left_behind_rows`], for a screen that may not show the paths into
/// heph's home (the report after the TUI, I9) when `paths` is false: no
/// `full log:` row (the `log:` row names the command), and `outputs:` points
/// at `heph inspect failure` instead of the slot's `out/`. A kept sandbox is
/// named in both: the user asked for it. Without `log_row`, no `log:` row at
/// all: a failure before the run has no log to name, or to say none of.
pub fn left_behind_rows_in(
    addr: &str,
    left: &LeftBehindData,
    paths: bool,
    log_row: bool,
) -> String {
    let mut out = String::new();
    if paths && let Some(path) = &left.log_path {
        out.push_str(&format!("  full log: {path}\n"));
    }
    if let Some(sandbox) = &left.sandbox {
        out.push_str(&format!("  sandbox: {sandbox}\n"));
    }
    if let Some(outputs) = &left.outputs {
        // What was kept, then where: the path, or the command that names it.
        let what = kept_groups(left);
        if paths && what.is_empty() {
            out.push_str(&format!("  outputs: {outputs}\n"));
        } else if paths {
            out.push_str(&format!("  outputs: {what} · {outputs}\n"));
        } else {
            let what = if what.is_empty() { "kept" } else { &what };
            out.push_str(&format!(
                "  outputs: {what} · heph inspect failure {}\n",
                shell_quote(addr)
            ));
        }
    }
    if let Some(fixes) = &left.fixes {
        fixes_rows(&mut out, fixes, (!paths).then_some(addr));
    }
    if !log_row {
        return out;
    }
    if log_readable(left) {
        out.push_str(&format!("  log: heph log {}\n", shell_quote(addr)));
    } else if left.log_readable == Some(false) {
        out.push_str("  log: none kept\n");
    }
    out
}

/// The kept `failure_out` groups, as a row names them: `report/ (3 files),
/// a.txt +2 more`. Empty when the slot counted none.
fn kept_groups(left: &LeftBehindData) -> String {
    let mut groups: Vec<String> = left
        .kept
        .iter()
        .map(|g| {
            let name = crate::tui::text::clean_inline(&g.name);
            match g.files {
                1 if !name.ends_with('/') => name.into_owned(),
                1 => format!("{name} (1 file)"),
                n => format!(
                    "{name} ({} files)",
                    hcore::units::thousands(usize::try_from(n).unwrap_or(usize::MAX))
                ),
            }
        })
        .collect();
    if left.kept_more > 0 {
        groups.push(format!("+{} more", left.kept_more));
    }
    groups.join(", ")
}

/// The `took down:` row of a culprit others failed because of: the first
/// few by name, then `+N more`. `None` when it took down nothing.
pub fn took_down_row(td: &hcore::events::TookDownData) -> Option<String> {
    if td.total == 0 {
        return None;
    }
    let mut named = td
        .named
        .iter()
        .map(|a| crate::tui::text::clean_inline(a).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let more = td
        .total
        .saturating_sub(u64::try_from(td.named.len()).unwrap_or(u64::MAX));
    if more > 0 {
        named.push_str(&format!(" +{more} more"));
    }
    Some(format!("took down: {named}"))
}

/// A path of more targets than this is elided in text (error-reporting U3,
/// E3): eliding fewer would hide a single hop.
const PATH_SHOWN_WHOLE: usize = 5;

/// A culprit's path in text: whole up to [`PATH_SHOWN_WHOLE`] targets, else
/// the requested target, the first hop and the last two, the middle counted
/// (`a → b → … 3 more (--json has the full path) … → f → g`). The last edge
/// is the one to edit, so it is always shown. `--json` carries the whole
/// path; only text elides it.
fn elide_path<S: AsRef<str>>(path: &[S]) -> String {
    let shown = |p: &[S]| {
        p.iter()
            .map(|a| crate::tui::text::clean_inline(a.as_ref()).into_owned())
            .collect::<Vec<_>>()
            .join(" → ")
    };
    match path_elision(path) {
        (head, 0, _) => shown(head),
        (head, hidden, tail) => format!(
            "{} → … {hidden} more (--json has the full path) … → {}",
            shown(head),
            shown(tail),
        ),
    }
}

/// The one elision rule, for text and for `--json`'s skipped records
/// (U23): `(head, hidden, tail)`, where a path of more than
/// [`PATH_SHOWN_WHOLE`] targets keeps the requested target and the first hop
/// in `head` and the last two in `tail`, `hidden` counting the targets
/// between. A shorter path is `(path, 0, [])`.
pub fn path_elision<T>(path: &[T]) -> (&[T], usize, &[T]) {
    if path.len() > PATH_SHOWN_WHOLE
        && let (Some(head), Some(tail)) = (path.get(..2), path.get(path.len().saturating_sub(2)..))
    {
        return (head, path.len().saturating_sub(4), tail);
    }
    (path, 0, &[])
}

/// The rows a culprit's box prints for how the request reached it, from its
/// `path` (from a requested target to the culprit, inclusive; for a source
/// file that did not evaluate, to the address that led into it), each
/// ending in a newline and without indentation:
/// - `reached by: //a:root → //b:x → //c:y`, elided as [`elide_path`] does;
///   none when the culprit was itself requested (a path of one);
/// - for a `file` culprit, the edge into it: `//c:y depends on //d:t, which
///   this file defines`, or `//d:t was requested, which this file defines`.
pub fn reached_by_rows<S: AsRef<str>>(path: &[S], file: bool) -> Vec<String> {
    let mut rows = Vec::new();
    let (to, entry) = match (file, path) {
        (true, [to @ .., entry]) => (to, Some(entry.as_ref())),
        _ => (path, None),
    };
    if to.len() > 1 {
        rows.push(format!("reached by: {}", elide_path(to)));
    }
    if let Some(entry) = entry {
        let entry = crate::tui::text::clean_inline(entry);
        rows.push(match to.last() {
            Some(from) => format!(
                "{} depends on {entry}, which this file defines",
                crate::tui::text::clean_inline(from.as_ref())
            ),
            None => format!("{entry} was requested, which this file defines"),
        });
    }
    rows
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
///   it there) and `readable` (`heph log` can open the log): the pointer
///   `(output not shown here: N lines · heph log //t)` and no line of the
///   tail (U5).
/// - Otherwise, a streamed tail whose log cannot be opened included (U26: a
///   pointer with no command to follow would leave nothing to read), the
///   tail's last `max_lines` lines, after an
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
    if streamed && readable {
        let note = format!(
            "(output not shown here: {} · heph log {})",
            lines(total, "line"),
            shell_quote(addr)
        );
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
/// `+N more (full list: where)`, or `(full list: where)` when every path
/// was named.
fn push_full_list(out: &mut String, more: usize, where_: &str) {
    if more > 0 {
        out.push_str(&format!("    +{more} more (full list: {where_})\n"));
    } else {
        out.push_str(&format!("    (full list: {where_})\n"));
    }
}

/// The write-back's rows. `inspect` is the failure's address on a screen
/// that names no path into heph's home (I9): the slot's fixes list is then
/// named by the command that prints it, never by its path.
fn fixes_rows(out: &mut String, fixes: &FixesData, inspect: Option<&str>) {
    const RESTORE: &str =
        "  restore with (takes the index version; uncommitted edits to these files are lost):\n";
    match fixes {
        FixesData::Written { paths, .. } if paths.is_empty() => {}
        FixesData::Written {
            root,
            paths,
            list,
            paths_more,
        } => {
            let git = format!("git -C {} --literal-pathspecs checkout", shell_quote(root));
            out.push_str(
                "  fixes written, target still failed (re-run to verify; \
                 codegen = \"in_place_on_success\" opts out):\n",
            );
            for p in paths.iter().take(FIXES_SHOWN) {
                out.push_str(&format!("    {}\n", crate::tui::text::clean_inline(p)));
            }
            // A record names the first paths and counts the rest (I8).
            let more = paths.len().saturating_sub(FIXES_SHOWN)
                + usize::try_from(*paths_more).unwrap_or(usize::MAX);
            // A path a terminal cannot show as typed goes through the list
            // too: the restore below would not paste back as written.
            let unprintable = paths.iter().any(|p| p.chars().any(char::is_control));
            match list {
                Some(_) if (more > 0 || unprintable) && inspect.is_some() => {
                    let addr = shell_quote(inspect.unwrap_or_default());
                    push_full_list(out, more, &format!("heph inspect failure {addr}"));
                    out.push_str(RESTORE);
                    out.push_str(&format!(
                        "    {git} --pathspec-from-file=<the fixes file heph inspect failure \
                         {addr} names>\n"
                    ));
                }
                Some(list) if more > 0 || unprintable => {
                    push_full_list(out, more, list);
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

    /// The `reached by:` rows of a target culprit whose path has `n`
    /// targets, named `a`, `b`, … (the culprit last).
    fn reached(n: u8) -> Vec<String> {
        let path: Vec<String> = (b'a'..b'a' + n)
            .map(|c| char::from(c).to_string())
            .collect();
        reached_by_rows(&path, false)
    }

    /// U3: a culprit that was itself requested has no `reached by:` row.
    #[test]
    fn elide_path_1_targets() {
        assert!(reached(1).is_empty());
    }

    #[test]
    fn elide_path_2_targets() {
        assert_eq!(reached(2), ["reached by: a → b"]);
    }

    #[test]
    fn elide_path_3_targets() {
        assert_eq!(reached(3), ["reached by: a → b → c"]);
    }

    #[test]
    fn elide_path_4_targets() {
        assert_eq!(reached(4), ["reached by: a → b → c → d"]);
    }

    /// Eliding five would hide one hop: printed whole.
    #[test]
    fn elide_path_5_targets() {
        assert_eq!(reached(5), ["reached by: a → b → c → d → e"]);
    }

    /// The requested target, the first hop and the last two; at least two
    /// hops hidden.
    #[test]
    fn elide_path_6_targets() {
        assert_eq!(
            reached(6),
            ["reached by: a → b → … 2 more (--json has the full path) … → e → f"]
        );
    }

    #[test]
    fn elide_path_7_targets() {
        assert_eq!(
            reached(7),
            ["reached by: a → b → … 3 more (--json has the full path) … → f → g"]
        );
    }

    /// U2: a file culprit's path ends at the address that led into it; the
    /// row names that edge, and `reached by:` stops at its declarer.
    #[test]
    fn file_culprit_rows_name_the_edge_into_the_file() {
        let path = ["//a:root", "//b:x", "//c:y", "//d:t"];
        assert_eq!(
            reached_by_rows(&path, true),
            [
                "reached by: //a:root → //b:x → //c:y",
                "//c:y depends on //d:t, which this file defines",
            ]
        );
        assert_eq!(
            reached_by_rows(&["//a:root", "//d:t"], true),
            ["//a:root depends on //d:t, which this file defines"]
        );
        assert_eq!(
            reached_by_rows(&["//d:t"], true),
            ["//d:t was requested, which this file defines"]
        );
        assert!(reached_by_rows::<&str>(&[], true).is_empty());
        assert!(reached_by_rows::<&str>(&[], false).is_empty());
    }

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
            ..LeftBehindData::default()
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

        // U26: no log to read, so no pointer: the streamed tail is shown.
        let r = tail_rows("//p:t", "a\nb\n", 999, true, false, 1000);
        assert_eq!(r.note.as_deref(), Some("… 998 earlier lines"));
        assert_eq!((r.shown, r.first_line), ("a\nb\n", 999));
        let r = tail_rows("//p:t", "a\nb\nc\n", 1, true, false, 2);
        assert_eq!(r.note.as_deref(), Some("… 1 earlier line"));
        assert_eq!(r.shown, "b\nc\n", "the streamed tail, cut to max_lines");
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
