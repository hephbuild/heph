//! The Log tab's pane: the last [`HELD_MAX`] bytes of one target's log, its
//! line starts, and where the view is in it. It follows the end until the
//! user scrolls; `/` searches only what it holds and says so. Text is
//! sanitized (I13) and heph's home redacted (I9) only for the lines a frame
//! draws, so a frame costs the visible window, not the log (I7).

use std::collections::VecDeque;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::text::{
    Redact, contains_ignore_ascii_case, cut_right, expand_tabs, sanitize_log, take_width, width,
};

/// The most of a log a pane holds: what search covers.
pub const HELD_MAX: usize = 256 * 1024;

/// Where the pane's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneState {
    /// Not asked for yet.
    Closed,
    /// Asked for; the reader has not answered.
    Opening,
    /// A run's log, followed as it is written.
    Live(String),
    /// The followed run ended.
    Ended(String),
    /// A finished log, read through.
    Read(String),
    /// No log, and why.
    Nothing(String),
    /// "log unavailable: …".
    Unavailable(String),
}

#[derive(Debug)]
pub struct LogPane {
    buf: VecDeque<u8>,
    /// Offset in the log of `buf`'s first byte.
    base: u64,
    /// Offsets in the log of each held line's start (the first may be cut).
    starts: VecDeque<u64>,
    /// Index among all the log's lines of `starts[0]`.
    first_line: u64,
    /// Whether line numbers are known: the log was read from its first byte.
    numbered: bool,
    /// The log's length as far as known.
    total: u64,
    pub state: PaneState,
    follow: bool,
    /// The line at the top while not following.
    top: u64,
    wrap: bool,
    query: String,
    editing: bool,
    /// The match the view was last moved to.
    cur: Option<u64>,
    note: Option<String>,
    /// The body rows of the last frame, for paging and leaving follow.
    rows: usize,
    /// The column the last frame's log text starts at, past the line
    /// numbers; `None` when it drew a notice instead of lines.
    text_col: Option<u16>,
}

impl Default for LogPane {
    fn default() -> Self {
        Self {
            buf: VecDeque::new(),
            base: 0,
            starts: VecDeque::new(),
            first_line: 0,
            numbered: true,
            total: 0,
            state: PaneState::Closed,
            follow: true,
            top: 0,
            wrap: false,
            query: String::new(),
            editing: false,
            cur: None,
            note: None,
            rows: 0,
            text_col: None,
        }
    }
}

impl LogPane {
    /// The column the last [`Self::body`]'s log text starts at (past the
    /// line numbers), or `None` when it drew no log lines.
    pub fn text_col(&self) -> Option<u16> {
        self.text_col
    }

    fn reset(&mut self, state: PaneState) {
        let (follow, wrap) = (self.follow, self.wrap);
        *self = Self {
            follow,
            wrap,
            state,
            ..Self::default()
        };
    }

    /// A run's log follows, from its first byte.
    pub fn start_live(&mut self, about: String) {
        self.reset(PaneState::Live(about));
    }

    /// A finished log of `len` bytes, of which `tail` are the last.
    pub fn start_tail(&mut self, tail: &[u8], len: u64, about: String) {
        self.reset(PaneState::Read(about));
        self.base = len.saturating_sub(tail.len() as u64);
        self.numbered = self.base == 0;
        self.push(tail);
        self.total = len.max(self.base + self.buf.len() as u64);
    }

    pub fn set_state(&mut self, state: PaneState) {
        self.reset(state);
    }

    /// The followed run ended: every byte was appended.
    pub fn ended(&mut self) {
        if let PaneState::Live(about) = &self.state {
            self.state = PaneState::Ended(about.clone());
        }
    }

    /// The next bytes of the log.
    pub fn append(&mut self, bytes: &[u8]) {
        self.push(bytes);
        self.total = self.total.saturating_add(bytes.len() as u64);
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.starts.is_empty() {
            self.starts.push_back(self.base);
        }
        let mut at = self.base + self.buf.len() as u64;
        for &b in bytes {
            self.buf.push_back(b);
            at += 1;
            if b == b'\n' {
                self.starts.push_back(at);
            }
        }
        self.trim();
    }

    /// Drop the front past [`HELD_MAX`], at a line start when one is in
    /// reach, else inside the one long line.
    fn trim(&mut self) {
        let len = self.buf.len();
        if len <= HELD_MAX {
            return;
        }
        let end = self.base + len as u64;
        let want = self.base + (len - HELD_MAX) as u64;
        let i = self.starts.partition_point(|&s| s < want);
        let cut = match self.starts.get(i) {
            Some(&s) if s < end => {
                self.starts.drain(..i);
                self.first_line += i as u64;
                s
            }
            _ => {
                // The line holding `want` is kept from `want` on.
                let keep = i.saturating_sub(1);
                self.starts.drain(..keep);
                self.first_line += keep as u64;
                if let Some(s) = self.starts.front_mut() {
                    *s = want;
                }
                want
            }
        };
        let n = usize::try_from(cut - self.base).unwrap_or(len);
        self.buf.drain(..n);
        self.base = cut;
    }

    /// Bytes before what is held: search and the counts cover a tail.
    pub fn is_cut(&self) -> bool {
        self.base > 0
    }

    /// Held lines; a newline at the end opens no line.
    pub fn line_count(&self) -> usize {
        let n = self.starts.len();
        match self.starts.back() {
            Some(&s) if s == self.base + self.buf.len() as u64 => n - 1,
            _ => n,
        }
    }

    fn line_bytes(&self, k: usize) -> Vec<u8> {
        self.line_range(k)
            .map(|r| self.buf.range(r).copied().collect())
            .unwrap_or_default()
    }

    /// Where held line `k` sits in `buf`, its newline left out.
    fn line_range(&self, k: usize) -> Option<std::ops::Range<usize>> {
        let &s = self.starts.get(k)?;
        let a = usize::try_from(s - self.base).unwrap_or(0);
        let b = self.starts.get(k + 1).map_or(self.buf.len(), |&e| {
            usize::try_from(e - self.base)
                .unwrap_or(0)
                .saturating_sub(1)
        });
        Some(a..b.max(a))
    }

    /// Whether held line `k` is only ASCII whitespace (a CR included), read
    /// in place. A line of controls alone is not blank here, and is dropped
    /// by the caller's check of its text.
    fn line_is_blank(&self, k: usize) -> bool {
        self.line_range(k)
            .is_none_or(|r| self.buf.range(r).all(u8::is_ascii_whitespace))
    }

    /// Held line `k` as drawable text: controls stripped, tabs laid out,
    /// heph's home redacted.
    fn line_text(&self, k: usize, redact: &Redact) -> String {
        let text = sanitize_log(&self.line_bytes(k));
        // The sanitized line itself when neither step changes it.
        let text = owned(redact.apply(&text)).unwrap_or(text);
        owned(expand_tabs(&text)).unwrap_or(text)
    }

    /// The last `n` non-blank lines, for the Overview.
    pub fn last_lines(&self, n: usize, redact: &Redact) -> Vec<String> {
        let mut out: Vec<String> = (0..self.line_count())
            .rev()
            // A blank line costs no allocation: a blank log is a frame's
            // worth of lines, every Overview frame.
            .filter(|&k| !self.line_is_blank(k))
            .map(|k| self.line_text(k, redact))
            .filter(|l| !l.trim().is_empty())
            .take(n)
            .collect();
        out.reverse();
        out
    }

    pub fn following(&self) -> bool {
        self.follow
    }

    /// Stop following and move `delta` lines.
    pub fn scroll(&mut self, delta: isize) {
        let count = self.line_count() as u64;
        let top = if self.follow {
            self.first_line + count.saturating_sub(self.rows as u64)
        } else {
            self.top
        };
        self.follow = false;
        let moved = if delta < 0 {
            top.saturating_sub(delta.unsigned_abs() as u64)
        } else {
            top.saturating_add(delta.unsigned_abs() as u64)
        };
        self.top = moved
            .max(self.first_line)
            .min(self.first_line + count.saturating_sub(1));
    }

    /// One screen up (`-1`) or down (`1`).
    pub fn page(&mut self, dir: isize) {
        let rows = isize::try_from(self.rows.max(1)).unwrap_or(1);
        self.scroll(dir * rows);
    }

    /// `f`, `G`, End: back to the end, following.
    pub fn follow_end(&mut self) {
        self.follow = true;
    }

    /// `g`, Home: the first held line.
    pub fn to_top(&mut self) {
        self.follow = false;
        self.top = self.first_line;
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
    }

    pub fn is_editing(&self) -> bool {
        self.editing
    }

    /// A search is typed or kept.
    pub fn has_search(&self) -> bool {
        self.editing || !self.query.is_empty()
    }

    pub fn search_start(&mut self) {
        self.editing = true;
        self.query.clear();
        self.note = None;
    }

    pub fn search_input(&mut self, c: char) {
        self.query.push(c);
    }

    pub fn search_backspace(&mut self) {
        self.query.pop();
    }

    pub fn search_cancel(&mut self) {
        self.editing = false;
        self.query.clear();
        self.note = None;
        self.cur = None;
    }

    /// Enter: search from the view, the last match when following.
    pub fn search_confirm(&mut self, redact: &Redact) {
        self.editing = false;
        if self.query.is_empty() {
            return;
        }
        let hits = self.hits(redact);
        let at = if self.follow {
            hits.last().copied()
        } else {
            hits.iter()
                .copied()
                .find(|&h| h >= self.top)
                .or_else(|| hits.first().copied())
        };
        self.land(&hits, at);
    }

    /// `n` (forward) / `N`: the next match, wrapping within what is held.
    pub fn search_next(&mut self, forward: bool, redact: &Redact) {
        if self.query.is_empty() {
            return;
        }
        let hits = self.hits(redact);
        let from = self.cur.unwrap_or(self.top);
        let at = if forward {
            hits.iter()
                .copied()
                .find(|&h| h > from)
                .or_else(|| hits.first().copied())
        } else {
            hits.iter()
                .rev()
                .copied()
                .find(|&h| h < from)
                .or_else(|| hits.last().copied())
        };
        self.land(&hits, at);
    }

    fn land(&mut self, hits: &[u64], at: Option<u64>) {
        let scope = if self.is_cut() {
            " in the last 256 KB"
        } else {
            ""
        };
        match at {
            Some(h) => {
                let i = hits.iter().position(|&x| x == h).map_or(0, |i| i + 1);
                self.note = Some(format!("match {i} of {}{scope}", hits.len()));
                self.cur = Some(h);
                self.follow = false;
                self.top = h
                    .saturating_sub((self.rows / 2) as u64)
                    .max(self.first_line);
            }
            None => {
                self.note = Some(format!("not found{scope}"));
                self.cur = None;
            }
        }
    }

    /// The lines holding the query, literal and ASCII case-insensitive.
    fn hits(&self, redact: &Redact) -> Vec<u64> {
        (0..self.line_count())
            .filter(|&k| contains_ignore_ascii_case(&self.line_text(k, redact), &self.query))
            .map(|k| self.first_line + k as u64)
            .collect()
    }

    /// The search prompt or its result, for the hint row.
    pub fn search_hint(&self) -> Option<String> {
        if self.editing {
            return Some(format!("/{}▏  enter search · esc clear", self.query));
        }
        if self.query.is_empty() {
            return None;
        }
        let note = self.note.as_deref().unwrap_or("");
        Some(format!(
            "/{} · {note} · n/N next/prev · esc clear",
            self.query
        ))
    }

    /// The tab row's right side: following or not, how much is held.
    pub fn status(&self) -> String {
        let count = self.line_count() as u64;
        let size = hcore::units::human_bytes(self.total);
        let amount = if self.is_cut() {
            format!(
                "last {} of {size}",
                hcore::units::human_bytes(self.buf.len() as u64)
            )
        } else {
            format!("{} lines · {size}", self.first_line + count)
        };
        match &self.state {
            PaneState::Live(_) if self.follow => format!("● following  {amount}"),
            PaneState::Live(_) => format!("paused (f follows)  {amount}"),
            PaneState::Ended(_) | PaneState::Read(_) => amount,
            _ => String::new(),
        }
    }

    /// The pane's body, `cols` × `rows`.
    pub fn body(&mut self, cols: usize, rows: usize, redact: &Redact) -> Vec<Line<'static>> {
        self.rows = rows;
        self.text_col = None;
        let count = self.line_count();
        let empty = match &self.state {
            PaneState::Closed | PaneState::Opening => Some("opening the log…".to_owned()),
            PaneState::Nothing(why) => Some(why.clone()),
            PaneState::Unavailable(why) => Some(why.clone()),
            PaneState::Live(_) if count == 0 => Some("no output yet".to_owned()),
            PaneState::Ended(_) | PaneState::Read(_) if count == 0 => Some("empty log".to_owned()),
            _ => None,
        };
        if let Some(text) = empty {
            let text = cut_right(&redact.apply(&text), cols.saturating_sub(1)).into_owned();
            return vec![Line::styled(
                format!(" {text}"),
                Style::default().fg(Color::DarkGray),
            )];
        }
        let gutter = if self.numbered {
            digits(self.first_line + count as u64)
        } else {
            0
        };
        let text_cols = cols.saturating_sub(gutter + 1).max(1);
        // Each row is the gutter, a space, then the text.
        self.text_col = u16::try_from(gutter + 1).ok();
        // The held lines this frame shows, as (line index, row texts).
        let mut shown: Vec<(usize, Vec<String>)> = Vec::new();
        let mut used = 0;
        if self.follow {
            for k in (0..count).rev() {
                let rows_of = self.rows_of(k, text_cols, redact);
                used += rows_of.len();
                shown.push((k, rows_of));
                if used >= rows {
                    break;
                }
            }
            shown.reverse();
            // Too many wrapped rows: drop the first line's leading rows.
            if let Some((_, first)) = shown.first_mut() {
                let extra = used.saturating_sub(rows).min(first.len());
                first.drain(..extra);
            }
            self.top = self.first_line + shown.first().map_or(0, |s| s.0) as u64;
        } else {
            let last = count.saturating_sub(rows.max(1)) as u64;
            let top_rel = self.top.saturating_sub(self.first_line).min(last);
            self.top = self.first_line + top_rel;
            let mut k = usize::try_from(top_rel).unwrap_or(0);
            while k < count && used < rows {
                let mut rows_of = self.rows_of(k, text_cols, redact);
                rows_of.truncate(rows - used);
                used += rows_of.len();
                shown.push((k, rows_of));
                k += 1;
            }
        }
        let mut out = Vec::with_capacity(rows);
        for (k, texts) in shown {
            let abs = self.first_line + k as u64;
            let style = if self.cur == Some(abs) {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            for (i, text) in texts.into_iter().enumerate() {
                let mut spans = Vec::with_capacity(2);
                if gutter > 0 {
                    let no = if i == 0 {
                        format!("{:>gutter$}", abs + 1)
                    } else {
                        " ".repeat(gutter)
                    };
                    spans.push(Span::styled(no, Style::default().fg(Color::DarkGray)));
                }
                spans.push(Span::styled(format!(" {text}"), style));
                out.push(Line::from(spans));
            }
        }
        out
    }

    /// Line `k`'s screen rows: one cut to `cols`, or all of it wrapped.
    fn rows_of(&self, k: usize, cols: usize, redact: &Redact) -> Vec<String> {
        let text = self.line_text(k, redact);
        if !self.wrap {
            return vec![cut_right(&text, cols).into_owned()];
        }
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let mut piece = take_width(rest, cols);
            if piece.is_empty() {
                // A character wider than the column: take it whole.
                piece = rest
                    .get(..rest.chars().next().map_or(0, char::len_utf8))
                    .unwrap_or(rest);
            }
            out.push(piece.to_owned());
            rest = rest.get(piece.len()..).unwrap_or("");
        }
        if out.is_empty() {
            out.push(String::new());
        }
        debug_assert!(out.iter().all(|r| width(r) <= cols.max(2)));
        out
    }
}

/// The new text when a step changed it; `None` when it lent its input back.
fn owned(text: std::borrow::Cow<'_, str>) -> Option<String> {
    match text {
        std::borrow::Cow::Owned(s) => Some(s),
        std::borrow::Cow::Borrowed(_) => None,
    }
}

fn digits(n: u64) -> usize {
    n.max(1).ilog10() as usize + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(p: &mut LogPane, cols: usize, rows: usize) -> Vec<String> {
        p.body(cols, rows, &Redact::default())
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .map(|s| s.trim().to_owned())
            .collect()
    }

    fn numbered(from: u32, to: u32) -> String {
        (from..=to).map(|i| format!("line{i}\n")).collect()
    }

    #[test]
    fn log_follows_until_scrolled() {
        let mut p = LogPane::default();
        p.start_live("live".into());
        p.append(numbered(1, 20).as_bytes());
        assert_eq!(
            texts(&mut p, 40, 3),
            ["18 line18", "19 line19", "20 line20"]
        );
        // Appended lines appear while following.
        p.append(b"line21\n");
        assert_eq!(
            texts(&mut p, 40, 3),
            ["19 line19", "20 line20", "21 line21"]
        );
        assert!(p.status().starts_with("● following"), "{}", p.status());
        // Scrolling up stops it: new lines do not move the view.
        p.scroll(-2);
        assert!(!p.following());
        assert_eq!(
            texts(&mut p, 40, 3),
            ["17 line17", "18 line18", "19 line19"]
        );
        p.append(b"line22\nline23\n");
        assert_eq!(
            texts(&mut p, 40, 3),
            ["17 line17", "18 line18", "19 line19"]
        );
        assert!(p.status().starts_with("paused"), "{}", p.status());
        // f resumes at the end.
        p.follow_end();
        assert_eq!(
            texts(&mut p, 40, 3),
            ["21 line21", "22 line22", "23 line23"]
        );
        // A line written in two chunks is one line.
        p.append(b"par");
        p.append(b"tial");
        assert_eq!(texts(&mut p, 40, 1), ["24 partial"]);
        // g goes to the top, G (End) back to following.
        p.to_top();
        assert_eq!(texts(&mut p, 40, 1), ["1 line1"]);
        p.follow_end();
        assert_eq!(texts(&mut p, 40, 1), ["24 partial"]);
    }

    #[test]
    fn log_search_bounded() {
        let r = Redact::default();
        // A 1 GB log whose only match is its first byte: the pane holds
        // the last 256 KB, and says so.
        let len: u64 = 1 << 30;
        let tail: Vec<u8> = b"x"
            .repeat(99)
            .into_iter()
            .chain(*b"\n")
            .cycle()
            .take(HELD_MAX)
            .collect();
        let mut p = LogPane::default();
        p.start_tail(&tail, len, "from the cache".into());
        assert!(p.is_cut());
        assert!(
            p.status().starts_with("last 256.0 KiB of 1.0 GiB"),
            "{}",
            p.status()
        );
        p.search_start();
        "Needle".chars().for_each(|c| p.search_input(c));
        p.search_confirm(&r);
        let hint = p.search_hint().expect("hint");
        assert!(hint.contains("not found in the last 256 KB"), "{hint}");

        // Followed past the cap: the first line, and its match, are gone.
        let mut p = LogPane::default();
        p.start_live("live".into());
        p.append(b"needle at the start\n");
        for _ in 0..40 {
            p.append(
                &b"y"
                    .repeat(9_999)
                    .into_iter()
                    .chain(*b"\n")
                    .collect::<Vec<_>>(),
            );
        }
        assert!(p.buf.len() <= HELD_MAX, "{}", p.buf.len());
        p.search_start();
        "needle".chars().for_each(|c| p.search_input(c));
        p.search_confirm(&r);
        assert!(
            p.search_hint()
                .expect("hint")
                .contains("not found in the last 256 KB")
        );

        // A match split across two chunks; n / N wrap within what is held;
        // literal and ASCII case-insensitive.
        let mut p = LogPane::default();
        p.start_live("live".into());
        p.append(b"a\nfound .* HE");
        p.append(b"RE\nb\nc\nfound .* here again\nd\nlast .* Here\n");
        let _ = texts(&mut p, 40, 2);
        p.search_start();
        ".* here".chars().for_each(|c| p.search_input(c));
        p.search_confirm(&r);
        // From the end while following: the last match.
        assert!(
            p.search_hint().expect("hint").contains("match 3 of 3 ·"),
            "{:?}",
            p.search_hint()
        );
        p.search_next(true, &r);
        assert!(
            p.search_hint().expect("hint").contains("match 1 of 3 ·"),
            "wraps forward"
        );
        p.search_next(false, &r);
        assert!(
            p.search_hint().expect("hint").contains("match 3 of 3 ·"),
            "wraps back"
        );
        p.search_next(false, &r);
        assert!(p.search_hint().expect("hint").contains("match 2 of 3 ·"));
        assert!(
            !p.search_hint().expect("hint").contains("256 KB"),
            "all held"
        );
        p.search_start();
        "absent".chars().for_each(|c| p.search_input(c));
        p.search_confirm(&r);
        assert!(p.search_hint().expect("hint").contains("· not found ·"));
    }

    /// Wrapped rows hold to the column count with an emoji whose
    /// presentation selector makes it wider than its characters sum to,
    /// and the Overview's last lines skip blank lines.
    #[test]
    fn wrapped_emoji_rows_fit_and_last_lines_skip_blanks() {
        let mut p = LogPane::default();
        p.start_live("live".into());
        p.append("⚠\u{fe0f}⚠\u{fe0f}⚠\u{fe0f} disk low\n\n  \n\n".as_bytes());
        p.toggle_wrap();
        for cols in 1..8 {
            for row in p.rows_of(0, cols, &Redact::default()) {
                assert!(width(&row) <= cols.max(2), "{row:?} at {cols}");
            }
        }
        assert_eq!(
            p.last_lines(3, &Redact::default()),
            ["⚠\u{fe0f}⚠\u{fe0f}⚠\u{fe0f} disk low"]
        );
    }

    /// The search reads the text the pane draws: heph's home redacted and
    /// tabs laid out, ASCII case folded; a line neither step changes is
    /// drawn as it was written.
    #[test]
    fn log_search_reads_the_drawn_text() {
        let r = Redact::of(["/hh/home".into()]);
        let mut p = LogPane::default();
        p.start_live("live".into());
        p.append(b"Plain line\nopen /hh/home/x\na\tb\n");
        let mut search = |q: &str| {
            p.search_start();
            q.chars().for_each(|c| p.search_input(c));
            p.search_confirm(&r);
            p.search_hint().expect("hint")
        };
        for (q, found) in [
            ("PLAIN LINE", true),
            ("<HEPH HOME>/x", true),
            ("/hh/home", false),
            ("a       b", true),
        ] {
            let hint = search(q);
            assert_eq!(hint.contains("match 1 of 1 ·"), found, "{q:?}: {hint}");
        }
        assert_eq!(p.line_text(0, &r), "Plain line");
        assert_eq!(p.line_text(1, &r), "open <heph home>/x");
        assert_eq!(p.line_text(2, &r), "a       b");
    }

    /// I13: the log-text table, drawn in the Log tab.
    #[test]
    fn control_bytes_never_reach_terminal_log_tab() {
        let long = "z".repeat(1 << 20);
        /// Name, chunks written, what the last drawn row ends with.
        type Case<'a> = (&'a str, Vec<&'a [u8]>, Option<&'a str>);
        let cases: Vec<Case<'_>> = vec![
            ("blank lines", vec![b"\n\n\n"], None),
            ("no newline", vec![b"last"], Some("last")),
            ("crlf", vec![b"one\r\n"], Some("one")),
            ("cr progress", vec![b"10%\r20%\r30%\n"], Some("30%")),
            (
                "modes",
                vec![b"a\x1b[?1049lb\x1b[?1003hc\x1b[2Jd\n"],
                Some("abcd"),
            ),
            (
                "osc",
                vec![b"\x1b]8;;http://x\x07link\x1b]8;;\x07 \x1b]52;c;Zm9v\x07ok\n"],
                Some("link ok"),
            ),
            ("lone csi", vec![b"x\x9by\n"], None),
            ("invalid utf8", vec![b"a\xffb\n"], Some("a\u{fffd}b")),
            ("nul", vec![b"a\0b\n"], Some("ab")),
            ("tabs", vec![b"a\tb\n"], Some("a       b")),
            ("long line", vec![long.as_bytes()], None),
            ("cut mid utf8", vec![b"caf\xc3", b"\xa9\n"], Some("café")),
            ("split csi", vec![b"x\x1b[", b"2Jy\n"], Some("xy")),
            (
                "split osc",
                vec![b"a\x1b]52;c;", b"Zm9v\x07b\n"],
                Some("ab"),
            ),
            ("spliced", vec![b"out: hal", b"err\n", b"f\n"], Some("f")),
            (
                "wide",
                vec!["日本語 e\u{301}\n".as_bytes()],
                Some("日本語 e\u{301}"),
            ),
        ];
        for (name, chunks, want_last) in cases {
            let mut p = LogPane::default();
            p.start_live("live".into());
            for c in chunks {
                p.append(c);
            }
            let lines = p.body(40, 5, &Redact::default());
            let drawn: String = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|s| s.content.as_ref())
                .collect();
            assert!(
                !drawn.chars().any(|c| c.is_control()),
                "{name}: control drawn: {drawn:?}"
            );
            for l in &lines {
                assert!(l.width() <= 40, "{name}: {} wide", l.width());
            }
            if let Some(want) = want_last {
                let last = lines.last().map(|l| l.to_string()).unwrap_or_default();
                assert!(last.trim_end().ends_with(want), "{name}: {last:?}");
            }
        }
        // Empty: says so, by state.
        let mut p = LogPane::default();
        p.start_live("live".into());
        assert_eq!(texts(&mut p, 40, 3), ["no output yet"]);
        p.ended();
        assert_eq!(texts(&mut p, 40, 3), ["empty log"]);
        // A wrapped long line stays within the column.
        let mut p = LogPane::default();
        p.start_live("live".into());
        p.append("日本語".repeat(20).as_bytes());
        p.toggle_wrap();
        for l in p.body(13, 10, &Redact::default()) {
            assert!(l.width() <= 13, "{}", l.width());
        }
    }
}
