//! The approval view: one pending request full screen, under the target
//! screens' header. Each notice the target declared is a tab; the whole
//! notice scrolls, nothing is cut. It is the only screen that decides: `y`/`n`
//! decide the request on screen by its number, `Y`/`N` set the standing
//! decision. Every decision key is locked out for [`LOCKOUT_MS`] after a
//! request first appears on screen, and a press then is dropped, not queued.
//!
//! A notice is read from the store by the sources recorded for it
//! ([`NoticeStore`], the one capped reader every viewer uses) on a thread of
//! its own, never on the render task, only once it is on screen, and drawn
//! sanitized (I13) with heph's home redacted (I9). A notice that cannot be
//! read shows [`hcore::approval_notice::NOTICE_UNAVAILABLE`], never a path.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use hcore::approval_notice::{NoticeRoots, NoticeSource, ReadNotice};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::chrome::{CHROME_ROWS, TABS_ROW, end_sub_frame, on_back, sub_tab_rows};
use super::header::{Title, header_top};
use super::model::Gauge;
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
/// The decision keys' place while the decision just taken shows: a key
/// pressed then is dropped, and this says so.
pub const FLASH_HINT: &str = "keys wait for the next request…";
/// An attached viewer's hint in their place: it decides nothing (I11).
pub const READ_ONLY_HINT: &str = "read only · decisions happen in the terminal that runs it";
/// The view's other keys.
pub const VIEW_HINTS: &str = "←/→ notice · ↑/↓ scroll · esc back";

/// Rows one wheel step moves.
const WHEEL_ROWS: isize = 3;

/// Where notices are read from: the run's store under `roots` (heph's home
/// and the workspace root), through `read` — the engine's
/// `read_notice_from_store`, which this crate cannot reach.
#[derive(Clone)]
pub struct NoticeStore {
    roots: NoticeRoots,
    read: ReadNotice,
}

impl std::fmt::Debug for NoticeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoticeStore")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

impl NoticeStore {
    pub fn new(roots: NoticeRoots, read: ReadNotice) -> Self {
        Self { roots, read }
    }

    /// The notice kept in `sources`, at most `NOTICE_READ_CAP` bytes plus the
    /// truncation line; `None` when a source is not where it was recorded or
    /// there is none. Blocking.
    pub fn read(&self, sources: &[NoticeSource]) -> Option<String> {
        if sources.is_empty() {
            return None;
        }
        (self.read)(&self.roots, sources)
    }
}

/// A notice's lines as a screen draws them, or why there are none.
#[derive(Debug, Clone)]
enum NoticeText {
    Reading,
    Unavailable,
    Lines(Vec<String>),
}

/// Which notice a text is: the request's number, and the notice's place
/// among the request's notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NoticeKey {
    seq: u64,
    index: usize,
}

/// The notice reader's thread: a notice's sources in, the drawable lines
/// out.
#[derive(Debug)]
struct NoticeFeed {
    ask: Sender<(NoticeKey, Vec<NoticeSource>)>,
    found: Receiver<(NoticeKey, Option<Vec<String>>)>,
}

impl NoticeFeed {
    fn spawn(store: NoticeStore, redact: Redact) -> Option<Self> {
        let (ask, todo) = mpsc::channel::<(NoticeKey, Vec<NoticeSource>)>();
        let (send, found) = mpsc::channel();
        std::thread::Builder::new()
            .name("heph-notice".into())
            .spawn(move || {
                while let Ok((key, sources)) = todo.recv() {
                    let lines = store.read(&sources).map(|text| drawable(&text, &redact));
                    if send.send((key, lines)).is_err() {
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
    texts: HashMap<NoticeKey, NoticeText>,
    feed: Option<NoticeFeed>,
    /// The current notice wrapped at a width: `(cols, notice, rows)`.
    wrapped: Option<(usize, usize, Vec<String>)>,
    /// The tab row's notice labels, by column, as the last frame drew them.
    tabs: Vec<(Range<u16>, usize)>,
    rows: u16,
    body_rows: usize,
    /// An attached viewer's: no decision key (see [`Self::read_only`]).
    read_only: bool,
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
            read_only: false,
        }
    }

    /// The number of the request on screen.
    pub fn seq(&self) -> u64 {
        self.req.seq
    }

    pub fn addr(&self) -> &str {
        &self.req.addr
    }

    /// This view in an attached viewer: it shows the request and its
    /// notices, offers no decision key and never arms; its hint says where
    /// the decision is made (I11).
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Whether a decision key acts at `now_ms`: the request has been on
    /// screen for [`LOCKOUT_MS`], and no decision is showing. Never in a
    /// [`Self::read_only`] view.
    pub fn armed(&self, now_ms: u64) -> bool {
        !self.read_only
            && self.flash.is_none()
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
        self.texts.retain(|k, _| {
            queue
                .iter()
                .any(|q| q.seq == k.seq && k.index < q.notices.len())
        });
        true
    }

    /// The key of the notice on screen, `None` when the request has none.
    fn notice_key(&self) -> Option<NoticeKey> {
        (self.notice < self.req.notices.len()).then_some(NoticeKey {
            seq: self.req.seq,
            index: self.notice,
        })
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
        if on_back(col) {
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
                    Ok((key, lines)) => {
                        let text = lines.map_or(NoticeText::Unavailable, NoticeText::Lines);
                        self.texts.insert(key, text);
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
        let (Some(key), Some(n)) = (self.notice_key(), self.req.notices.get(self.notice)) else {
            return;
        };
        if self.texts.contains_key(&key) {
            return;
        }
        // A notice that names no source is unavailable without asking.
        let asked = !n.sources.is_empty()
            && self
                .feed
                .as_ref()
                .is_some_and(|f| f.ask.send((key, n.sources.clone())).is_ok());
        let text = if asked {
            NoticeText::Reading
        } else {
            NoticeText::Unavailable
        };
        self.texts.insert(key, text);
    }

    /// The body: the current notice wrapped to `cols`, or why it is not
    /// there.
    fn body(&mut self, cols: usize) -> Vec<Line<'static>> {
        let dim = Style::default().fg(Color::DarkGray);
        let Some(key) = self.notice_key() else {
            return vec![Line::styled("  this target declares no notice", dim)];
        };
        match self.texts.get(&key) {
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
        let (header, _) = header_top(inp.title, inp.gauges, cols);
        let mut out: Vec<Line<'static>> = header.into_iter().collect();
        let c = usize::from(cols);
        let body_rows = usize::from(rows.saturating_sub(CHROME_ROWS));
        let body = self.body(c);
        self.scroll = self.scroll.min(body.len().saturating_sub(body_rows));
        self.body_rows = body_rows;
        let (tabs, rule) = self.tab_rows(inp.glyph, inp.queue, c);
        out.push(tabs);
        out.push(rule);
        self.rows = rows;
        out.extend(body.into_iter().skip(self.scroll).take(body_rows));
        end_sub_frame(&mut out, self.hints(inp.now_ms, c), rows);
        out
    }

    /// ` ◂ esc  ✦ //addr  │  plan    changelog      1 of 2 pending` and the
    /// rule under it, heavy under the notice on screen. The status is the
    /// decision just taken while it shows, else where the request is in
    /// `queue`.
    fn tab_rows(
        &mut self,
        glyph: &'static str,
        queue: &[QueuedApproval],
        cols: usize,
    ) -> (Line<'static>, Line<'static>) {
        let pending;
        let status = match &self.flash {
            Some(f) => (
                f.text.as_str(),
                Style::default()
                    .fg(if f.approved { Color::Green } else { Color::Red })
                    .add_modifier(Modifier::BOLD),
            ),
            None => {
                let at = queue.iter().position(|q| q.seq == self.req.seq);
                pending = match at {
                    Some(k) => format!("{} of {} pending", k + 1, queue.len()),
                    None => format!("{} pending", queue.len()),
                };
                (pending.as_str(), Style::default().fg(Color::DarkGray))
            }
        };
        let addr = cut_left(&clean_inline(&self.req.addr), cols / 3).into_owned();
        let crumb = [
            Span::styled(
                glyph,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(addr, Style::default().add_modifier(Modifier::BOLD)),
        ];
        let names = self
            .req
            .notices
            .iter()
            .map(|n| clean_inline(&n.name).into_owned());
        let hits = &mut self.tabs;
        hits.clear();
        sub_tab_rows(crumb, names, self.notice, status, cols, |i, at| {
            hits.push((at, i));
        })
    }

    /// The hint row: the decision keys, greyed out while they are locked
    /// out (no countdown), then the view's own keys.
    fn hints(&self, now_ms: u64, cols: usize) -> Line<'static> {
        let room = cols.saturating_sub(2);
        let keys = if self.read_only {
            READ_ONLY_HINT
        } else if self.flash.is_some() {
            FLASH_HINT
        } else {
            DECISION_HINTS
        };
        let decision = cut_right(keys, room).into_owned();
        let left = room.saturating_sub(width(&decision));
        // A read-only hint is never greyed: it is not a key that arms.
        let decision_style = if self.read_only || self.armed(now_ms) {
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

/// A store as the tests keep one, in place of the engine's reader, which
/// this crate cannot reach: each source a file under the workspace root,
/// read only while it still hashes to the `hashout` recorded for it.
#[cfg(test)]
pub(crate) mod test_store {
    use std::hash::{Hash as _, Hasher as _};
    use std::path::{Path, PathBuf};

    use hcore::approval_notice::{NoticeOrigin, NoticeRoots, NoticeSource};

    use crate::tui::approval::ApprovalNoticeView;

    fn digest(text: &str) -> String {
        let mut h = std::hash::DefaultHasher::new();
        text.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    /// The reader: every source's file, concatenated; `None` when one is
    /// missing or changed. Matches [`hcore::approval_notice::ReadNotice`].
    pub fn read(roots: &NoticeRoots, sources: &[NoticeSource]) -> Option<String> {
        let mut text = String::new();
        for s in sources {
            let NoticeOrigin::File { path, hashout, .. } = &s.origin else {
                return None;
            };
            let body = std::fs::read_to_string(roots.root.join(path)).ok()?;
            if digest(&body) != *hashout {
                return None;
            }
            text.push_str(&body);
        }
        Some(text)
    }

    /// The roots of a run whose home and workspace are both `dir`.
    pub fn roots(dir: &Path) -> NoticeRoots {
        NoticeRoots {
            home: dir.to_path_buf(),
            root: dir.to_path_buf(),
        }
    }

    /// Where [`kept`] keeps notice `name` under `dir`, to remove or rewrite.
    pub fn file(dir: &Path, name: &str) -> PathBuf {
        dir.join("store").join(name)
    }

    /// Notice `name`, its text `content` kept in the store under `dir`.
    pub fn kept(dir: &Path, name: &str, content: &str) -> ApprovalNoticeView {
        let path = file(dir, name);
        std::fs::create_dir_all(dir.join("store")).expect("store dir");
        std::fs::write(&path, content).expect("notice source");
        let rel = format!("store/{name}");
        ApprovalNoticeView {
            name: name.into(),
            sources: vec![NoticeSource {
                addr: format!("//n:{name}"),
                origin: NoticeOrigin::File {
                    path: rel.clone(),
                    out_path: rel,
                    x: false,
                    hashout: digest(content),
                },
            }],
        }
    }
}
