//! The approval view: one pending request full screen, under the target
//! screens' header. Each notice the target declared is a tab; the whole
//! notice scrolls, nothing is cut. It is the only screen that decides: `y`/`n`
//! decide the request on screen by its number, `Y`/`N` set the standing
//! decision. Every decision key is locked out for [`LOCKOUT_MS`] after a
//! request first appears on screen, and a press then is dropped, not queued.
//!
//! A notice is read by its content hash ([`NoticeStore`]) on a thread of its
//! own, never on the render task, and drawn sanitized (I13) with heph's home
//! redacted (I9). A notice that cannot be read shows
//! [`hcore::approval_notice::NOTICE_UNAVAILABLE`], never a path.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::header::{Title, header_lines};
use super::model::{Gauge, Tab, TabCounts};
use crate::tui::approval::QueuedApproval;
use crate::tui::text::{
    Redact, clean_inline, cut_left, cut_right, expand_tabs, sanitize_log, take_width, width,
};

/// How long a request is on screen before a decision key acts on it.
pub const LOCKOUT_MS: u64 = 500;
/// How long the header shows the decision just taken before moving on.
pub const FLASH_MS: u64 = 1000;

/// The decision keys' hint, greyed out during the lockout.
pub const DECISION_HINTS: &str =
    "y approve · n reject · Y approves all, and later · N rejects all, and later (fails them)";
/// The view's other keys.
pub const VIEW_HINTS: &str = "←/→ notice · ↑/↓ scroll · esc back";

/// The breadcrumb that starts the tab row; a click on it goes back.
const BACK: &str = " ◂ esc  ";
/// Header (2), tab row, rule, a blank and the hint row.
const CHROME_ROWS: u16 = 6;
const TABS_ROW: u16 = 2;
/// Rows one wheel step moves.
const WHEEL_ROWS: isize = 3;

/// Where notices are read from: heph's home's content-addressed store.
#[derive(Debug, Clone)]
pub struct NoticeStore {
    home: PathBuf,
}

impl NoticeStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// The text stored under `hash`, checked against it; `None` when it is
    /// gone, does not match or no hash was recorded. Blocking.
    pub fn read(&self, hash: Option<&str>) -> Option<String> {
        hcore::approval_notice::read_notice_blob(&self.home, hash)
    }
}

/// A notice's lines as a screen draws them, or why there are none.
#[derive(Debug, Clone)]
enum NoticeText {
    Reading,
    Unavailable,
    Lines(Vec<String>),
}

/// The notice reader's thread: a hash in, the drawable lines out.
#[derive(Debug)]
struct NoticeFeed {
    ask: Sender<Option<String>>,
    found: Receiver<(Option<String>, Option<Vec<String>>)>,
}

impl NoticeFeed {
    fn spawn(store: NoticeStore, redact: Redact) -> Option<Self> {
        let (ask, todo) = mpsc::channel::<Option<String>>();
        let (send, found) = mpsc::channel();
        std::thread::Builder::new()
            .name("heph-notice".into())
            .spawn(move || {
                while let Ok(hash) = todo.recv() {
                    let lines = store
                        .read(hash.as_deref())
                        .map(|text| drawable(&text, &redact));
                    if send.send((hash, lines)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| tracing::debug!(error = %e, "notice reader unavailable"))
            .ok()?;
        Some(Self { ask, found })
    }
}

/// Notice text as lines a cell grid can draw (I13), heph's home redacted
/// (I9).
fn drawable(text: &str, redact: &Redact) -> Vec<String> {
    sanitize_log(text.as_bytes())
        .split('\n')
        .map(|l| redact.apply(&expand_tabs(l)).into_owned())
        .collect()
}

/// `line` wrapped to `cols`, each piece no wider: a long line shows whole
/// on several rows.
fn wrap(line: &str, cols: usize, out: &mut Vec<String>) {
    let mut rest = line;
    if rest.is_empty() {
        out.push(String::new());
    }
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
}

/// The decision the header shows for [`FLASH_MS`].
#[derive(Debug, Clone)]
struct Flash {
    text: String,
    approved: bool,
    until_ms: u64,
}

/// What a click on the approval view did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalClick {
    /// The breadcrumb: back, like esc.
    Back,
    /// A notice tab switched.
    Done,
    Nothing,
}

/// What a frame of the approval view reads.
pub struct ApprovalInputs<'a> {
    pub title: &'a Title<'a>,
    pub gauges: &'a [&'a Gauge],
    pub counts: &'a TabCounts,
    /// Every pending request, oldest first.
    pub queue: &'a [QueuedApproval],
    /// The approval glyph this frame (`✦`/`❖`).
    pub glyph: &'static str,
    pub now_ms: u64,
}

/// The approval view's state: the request on screen and how it is shown.
#[derive(Debug)]
pub struct ApprovalScreen {
    req: QueuedApproval,
    notice: usize,
    scroll: usize,
    /// When a frame first drew this request: the lockout's start.
    shown_since: Option<u64>,
    /// The notices drawn, by index: what a `y`/`n` line says was read.
    read: Vec<bool>,
    flash: Option<Flash>,
    texts: HashMap<Option<String>, NoticeText>,
    feed: Option<NoticeFeed>,
    /// The current notice wrapped at a width: `(cols, notice, rows)`.
    wrapped: Option<(usize, usize, Vec<String>)>,
    /// The tab row's notice labels, by column, as the last frame drew them.
    tabs: Vec<(Range<u16>, usize)>,
    rows: u16,
    body_rows: usize,
}

impl ApprovalScreen {
    /// The view on `req`, its first notice up. Its notices are read from
    /// `store` (none: every notice reads "notice unavailable").
    pub fn open(req: QueuedApproval, store: Option<NoticeStore>, redact: &Redact) -> Self {
        let feed = store.and_then(|s| NoticeFeed::spawn(s, redact.clone()));
        let read = vec![false; req.notices.len()];
        Self {
            req,
            notice: 0,
            scroll: 0,
            shown_since: None,
            read,
            flash: None,
            texts: HashMap::new(),
            feed,
            wrapped: None,
            tabs: Vec::new(),
            rows: 0,
            body_rows: 0,
        }
    }

    /// The number of the request on screen.
    pub fn seq(&self) -> u64 {
        self.req.seq
    }

    pub fn addr(&self) -> &str {
        &self.req.addr
    }

    /// Whether a decision key acts at `now_ms`: the request has been on
    /// screen for [`LOCKOUT_MS`], and no decision is showing.
    pub fn armed(&self, now_ms: u64) -> bool {
        self.flash.is_none()
            && self
                .shown_since
                .is_some_and(|t| now_ms.saturating_sub(t) >= LOCKOUT_MS)
    }

    /// The names of the notices this request showed, for its decision line.
    pub fn read_names(&self) -> Vec<String> {
        self.req
            .notices
            .iter()
            .zip(&self.read)
            .filter(|&(_, &r)| r)
            .map(|(n, _)| n.name.clone())
            .collect()
    }

    /// The request on screen was decided: the header shows it for
    /// [`FLASH_MS`], then the view moves on.
    pub fn decided(&mut self, approved: bool, now_ms: u64) {
        let addr = clean_inline(&self.req.addr).into_owned();
        let text = if approved {
            format!("✓ approved {addr}")
        } else {
            format!("✗ rejected {addr}")
        };
        self.flash = Some(Flash {
            text,
            approved,
            until_ms: now_ms.saturating_add(FLASH_MS),
        });
    }

    /// Move on once the decision has shown: to the oldest pending request,
    /// on its first notice, with its own lockout. A request on screen that
    /// left the queue undecided here (a standing decision, a cancel) moves
    /// on at once. `false`: none is left, the view closes.
    pub fn advance(&mut self, now_ms: u64, queue: &[QueuedApproval]) -> bool {
        match &self.flash {
            Some(f) if now_ms < f.until_ms => return true,
            Some(_) => self.flash = None,
            None if queue.iter().any(|q| q.seq == self.req.seq) => return true,
            None => {}
        }
        let Some(next) = queue.first() else {
            return false;
        };
        self.req = next.clone();
        self.notice = 0;
        self.scroll = 0;
        self.shown_since = None;
        self.read = vec![false; self.req.notices.len()];
        self.wrapped = None;
        // Notices of requests no longer queued are not read again.
        let keep: Vec<Option<String>> = queue
            .iter()
            .flat_map(|q| q.notices.iter().map(|n| n.hash.clone()))
            .collect();
        self.texts.retain(|h, _| keep.contains(h));
        true
    }

    /// ←/→ (and tab): the next or previous notice. The lockout runs on.
    pub fn cycle_notice(&mut self, forward: bool) {
        let n = self.req.notices.len();
        if n == 0 {
            return;
        }
        self.notice = if forward {
            (self.notice + 1) % n
        } else {
            (self.notice + n - 1) % n
        };
        self.scroll = 0;
    }

    /// ↑/↓ and the page keys.
    pub fn scroll(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }

    pub fn page(&self) -> isize {
        isize::try_from(self.body_rows.max(1)).unwrap_or(1)
    }

    pub fn wheel(&mut self, down: bool) {
        self.scroll(if down { WHEEL_ROWS } else { -WHEEL_ROWS });
    }

    /// A click at `(col, row)` of the last frame: the breadcrumb or a
    /// notice tab. Nothing on this screen decides.
    pub fn click(&mut self, col: u16, row: u16) -> ApprovalClick {
        if row != TABS_ROW || self.rows <= TABS_ROW + 1 {
            return ApprovalClick::Nothing;
        }
        if usize::from(col) < width(BACK.trim_end()) {
            return ApprovalClick::Back;
        }
        match self.tabs.iter().find(|(cols, _)| cols.contains(&col)) {
            Some(&(_, i)) => {
                if i != self.notice {
                    self.notice = i;
                    self.scroll = 0;
                }
                ApprovalClick::Done
            }
            None => ApprovalClick::Nothing,
        }
    }

    /// Take what the reader found, and ask for the notice on screen.
    fn pump(&mut self) {
        if let Some(feed) = &self.feed {
            loop {
                match feed.found.try_recv() {
                    Ok((hash, lines)) => {
                        let text = lines.map_or(NoticeText::Unavailable, NoticeText::Lines);
                        self.texts.insert(hash, text);
                    }
                    Err(TryRecvError::Empty) => break,
                    // The thread is gone: what it was reading never comes.
                    Err(TryRecvError::Disconnected) => {
                        self.feed = None;
                        for t in self.texts.values_mut() {
                            if matches!(t, NoticeText::Reading) {
                                *t = NoticeText::Unavailable;
                            }
                        }
                        break;
                    }
                }
            }
        }
        let Some(hash) = self.req.notices.get(self.notice).map(|n| n.hash.clone()) else {
            return;
        };
        if self.texts.contains_key(&hash) {
            return;
        }
        let asked = hash.is_some()
            && self
                .feed
                .as_ref()
                .is_some_and(|f| f.ask.send(hash.clone()).is_ok());
        let text = if asked {
            NoticeText::Reading
        } else {
            NoticeText::Unavailable
        };
        self.texts.insert(hash, text);
    }

    /// The body: the current notice wrapped to `cols`, or why it is not
    /// there.
    fn body(&mut self, cols: usize) -> Vec<Line<'static>> {
        let dim = Style::default().fg(Color::DarkGray);
        let Some(n) = self.req.notices.get(self.notice) else {
            return vec![Line::styled("  this target declares no notice", dim)];
        };
        match self.texts.get(&n.hash) {
            None | Some(NoticeText::Reading) => vec![Line::styled("  reading the notice…", dim)],
            Some(NoticeText::Unavailable) => vec![Line::styled(
                format!("  {}", hcore::approval_notice::NOTICE_UNAVAILABLE),
                Style::default().fg(Color::Yellow),
            )],
            Some(NoticeText::Lines(lines)) => {
                if let Some(r) = self.read.get_mut(self.notice) {
                    *r = true;
                }
                let room = cols.saturating_sub(2).max(1);
                let fresh =
                    !matches!(&self.wrapped, Some((c, k, _)) if *c == room && *k == self.notice);
                if fresh {
                    let mut rows = Vec::with_capacity(lines.len());
                    for l in lines {
                        wrap(l, room, &mut rows);
                    }
                    // A trailing newline is not a row of its own.
                    if rows.len() > 1 && rows.last().is_some_and(String::is_empty) {
                        rows.pop();
                    }
                    self.wrapped = Some((room, self.notice, rows));
                }
                self.wrapped
                    .as_ref()
                    .map(|(_, _, rows)| rows.iter().map(|r| Line::raw(format!("  {r}"))).collect())
                    .unwrap_or_default()
            }
        }
    }

    /// The view at `cols` × `rows`: the dashboard header's first two rows,
    /// the notice tab row and its rule, the notice, a blank and the hints.
    pub fn frame(&mut self, inp: &ApprovalInputs<'_>, cols: u16, rows: u16) -> Vec<Line<'static>> {
        if self.shown_since.is_none() && self.flash.is_none() {
            self.shown_since = Some(inp.now_ms);
        }
        self.pump();
        let (header, _) = header_lines(inp.title, inp.gauges, inp.counts, Tab::All, cols);
        let mut out: Vec<Line<'static>> = header.into_iter().take(2).collect();
        let c = usize::from(cols);
        let body_rows = usize::from(rows.saturating_sub(CHROME_ROWS));
        let body = self.body(c);
        self.scroll = self.scroll.min(body.len().saturating_sub(body_rows));
        self.body_rows = body_rows;
        let status = match &self.flash {
            Some(f) => (
                f.text.clone(),
                Style::default()
                    .fg(if f.approved { Color::Green } else { Color::Red })
                    .add_modifier(Modifier::BOLD),
            ),
            None => {
                let at = inp.queue.iter().position(|q| q.seq == self.req.seq);
                let text = match at {
                    Some(k) => format!("{} of {} pending", k + 1, inp.queue.len()),
                    None => format!("{} pending", inp.queue.len()),
                };
                (text, Style::default().fg(Color::DarkGray))
            }
        };
        let (tabs, rule) = self.tab_rows(inp.glyph, status, c);
        out.push(tabs);
        out.push(rule);
        self.rows = rows;
        out.extend(body.into_iter().skip(self.scroll).take(body_rows));
        while out.len() < usize::from(rows.saturating_sub(2)) {
            out.push(Line::default());
        }
        out.push(Line::default());
        out.push(self.hints(inp.now_ms, c));
        let rows = usize::from(rows);
        if out.len() > rows {
            let hint = out.pop();
            out.truncate(rows.saturating_sub(1));
            out.extend(hint.filter(|_| rows > 0));
        }
        out
    }

    /// ` ◂ esc  ✦ //addr  │  plan    changelog      1 of 2 pending` and the
    /// rule under it, heavy under the notice on screen.
    fn tab_rows(
        &mut self,
        glyph: &'static str,
        (status, status_style): (String, Style),
        cols: usize,
    ) -> (Line<'static>, Line<'static>) {
        let dim = Style::default().fg(Color::DarkGray);
        let addr = cut_left(&clean_inline(&self.req.addr), cols / 3).into_owned();
        let mut spans = vec![
            Span::styled(BACK, dim),
            Span::styled(
                glyph,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(addr, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("  │  ", dim),
        ];
        let mut at: usize = spans.iter().map(|s| width(&s.content)).sum();
        let mut mark = (0, 0);
        self.tabs.clear();
        for (i, n) in self.req.notices.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("    "));
                at += 4;
            }
            let name = clean_inline(&n.name).into_owned();
            let w = width(&name);
            let col = |c: usize| u16::try_from(c.min(cols)).unwrap_or(u16::MAX);
            self.tabs.push((col(at)..col(at + w), i));
            if i == self.notice {
                mark = (at, w);
                spans.push(Span::styled(
                    name,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(name, dim));
            }
            at += w;
        }
        let room = cols.saturating_sub(at + 2);
        if room > 4 {
            let status = cut_right(&status, room).into_owned();
            let gap = cols.saturating_sub(at + width(&status) + 1);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(status, status_style));
        }
        let mut line = Line::from(spans);
        if line.width() > cols {
            line = Line::raw(cut_right(&line.to_string(), cols).into_owned());
        }
        let (start, w) = mark;
        let rule: String = (0..cols)
            .map(|i| {
                if i >= start && i < start + w {
                    '━'
                } else {
                    '─'
                }
            })
            .collect();
        (line, Line::styled(rule, dim))
    }

    /// The hint row: the decision keys, greyed out while they are locked
    /// out (no countdown), then the view's own keys.
    fn hints(&self, now_ms: u64, cols: usize) -> Line<'static> {
        let room = cols.saturating_sub(2);
        let decision = cut_right(DECISION_HINTS, room).into_owned();
        let left = room.saturating_sub(width(&decision));
        let decision_style = if self.armed(now_ms) {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::raw("  "), Span::styled(decision, decision_style)];
        if left > 3 {
            let rest = cut_right(VIEW_HINTS, left - 3).into_owned();
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(rest, Style::default().fg(Color::DarkGray)));
        }
        Line::from(spans)
    }
}
