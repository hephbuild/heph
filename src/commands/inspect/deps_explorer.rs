//! Interactive dependency explorer (`heph inspect deps //a:b -i`).
//!
//! The current target's selectable dependency list fills the screen; the drill
//! trail accumulates in a panel at the bottom (root → current, one indented row
//! per level, growing as you descend). Left/Enter drills into the highlighted
//! dep (it becomes the new current level and its deps fill the list);
//! Right/Esc walks back up. `/` filters the active list.
//!
//! The state machine ([`Explorer`]) is engine-free and unit-tested; the engine
//! only supplies [`NodeInfo`] when a level is first visited.

use std::io;
use std::sync::Arc;

use anyhow::Context;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::commands::bootstrap::ShutdownTrigger;
use crate::engine::Engine;
use crate::htaddr::Addr;
use crate::tui::LogSink;

/// Summary of a target, captured the first time a level is visited.
#[derive(Clone)]
pub(crate) struct NodeInfo {
    /// Deduplicated dependency addresses (target inputs), in declared order.
    pub deps: Vec<Addr>,
}

/// One column in the drill chain: a target plus its dependency list cursor.
struct Level {
    addr: Addr,
    info: NodeInfo,
    /// Highlighted index into `info.deps`.
    sel: usize,
    /// First visible row of the dep list (vertical scroll offset).
    voff: usize,
}

/// Active incremental-search state over the current level's dep list. While
/// `Some`, the active list shows only deps whose address matches `query`
/// (case-insensitive substring) and `sel`/`voff` index into that filtered view.
struct Search {
    query: String,
    sel: usize,
    voff: usize,
}

/// The explorer's navigation state. `path[0]` is the root; `path.last()` is the
/// current target whose deps are the active list.
pub(crate) struct Explorer {
    path: Vec<Level>,
    /// Transient one-line status (e.g. a fetch error), cleared on next nav.
    status: Option<String>,
    /// `Some` while `/`-search is filtering the current level's dep list.
    search: Option<Search>,
    /// `Some(addr)` while a drill into `addr` is being fetched — drives the
    /// loading spinner and suppresses further drills until it resolves.
    loading: Option<Addr>,
}

impl Explorer {
    pub(crate) fn new(root: Addr, info: NodeInfo) -> Self {
        Self {
            path: vec![Level {
                addr: root,
                info,
                sel: 0,
                voff: 0,
            }],
            status: None,
            search: None,
            loading: None,
        }
    }

    fn current(&self) -> &Level {
        // `path` is never empty: constructed with one level, `back` keeps >= 1.
        self.path.last().expect("explorer path is never empty")
    }

    fn current_mut(&mut self) -> &mut Level {
        self.path.last_mut().expect("explorer path is never empty")
    }

    /// Indices into the current level's `deps` that are currently visible —
    /// every dep, or just the search matches when a search is active.
    fn visible(&self) -> Vec<usize> {
        let deps = &self.current().info.deps;
        match &self.search {
            Some(s) => {
                let q = s.query.to_lowercase();
                deps.iter()
                    .enumerate()
                    .filter(|(_, a)| a.format().to_lowercase().contains(&q))
                    .map(|(i, _)| i)
                    .collect()
            }
            None => (0..deps.len()).collect(),
        }
    }

    /// Cursor position within the visible (possibly filtered) list.
    fn cursor(&self) -> usize {
        match &self.search {
            Some(s) => s.sel,
            None => self.current().sel,
        }
    }

    pub(crate) fn up(&mut self) {
        self.status = None;
        if let Some(s) = &mut self.search {
            s.sel = s.sel.saturating_sub(1);
        } else {
            let cur = self.current_mut();
            cur.sel = cur.sel.saturating_sub(1);
        }
    }

    pub(crate) fn down(&mut self) {
        self.status = None;
        let len = self.visible().len();
        if let Some(s) = &mut self.search {
            if s.sel + 1 < len {
                s.sel += 1;
            }
        } else {
            let cur = self.current_mut();
            if cur.sel + 1 < len {
                cur.sel += 1;
            }
        }
    }

    /// The dep currently highlighted in the active list, if any.
    pub(crate) fn selected(&self) -> Option<Addr> {
        let deps = &self.current().info.deps;
        self.visible()
            .get(self.cursor())
            .and_then(|&i| deps.get(i))
            .cloned()
    }

    /// Drill into `dep` (already fetched into `info`): push a new current level.
    pub(crate) fn drill(&mut self, dep: Addr, info: NodeInfo) {
        self.status = None;
        self.search = None;
        self.path.push(Level {
            addr: dep,
            info,
            sel: 0,
            voff: 0,
        });
    }

    /// Walk back up toward the root. Returns false if already at the root.
    pub(crate) fn back(&mut self) -> bool {
        self.status = None;
        self.search = None;
        if self.path.len() > 1 {
            self.path.pop();
            true
        } else {
            false
        }
    }

    /// Begin `/`-search on the current level (no-op if already searching).
    pub(crate) fn start_search(&mut self) {
        self.status = None;
        if self.search.is_none() {
            self.search = Some(Search {
                query: String::new(),
                sel: 0,
                voff: 0,
            });
        }
    }

    /// Cancel search and restore the full dep list.
    pub(crate) fn cancel_search(&mut self) {
        self.search = None;
    }

    fn searching(&self) -> bool {
        self.search.is_some()
    }

    pub(crate) fn search_push(&mut self, c: char) {
        if let Some(s) = &mut self.search {
            s.query.push(c);
            s.sel = 0;
            s.voff = 0;
        }
    }

    pub(crate) fn search_backspace(&mut self) {
        if let Some(s) = &mut self.search {
            s.query.pop();
            s.sel = 0;
            s.voff = 0;
        }
    }

    /// Whether `addr` already sits on the current drill chain (a dependency
    /// cycle, or a diamond revisit) — surfaced so the user isn't misled.
    fn on_path(&self, addr: &Addr) -> bool {
        self.path.iter().any(|l| &l.addr == addr)
    }
}

/// Collect a target's *direct* deps for one level. Dedups deps by address
/// (multiple inputs may reference the same target via different outputs).
///
/// Uses `get_direct_def` (no transitive resolution) so each drill fetches only
/// the next level — entering a dep is what pulls its children, never the whole
/// transitive closure up front.
async fn fetch_node(engine: &Arc<Engine>, addr: &Addr) -> anyhow::Result<NodeInfo> {
    let rs = engine.new_state();
    let def = engine
        .clone()
        .get_direct_def(rs, addr)
        .await
        .with_context(|| format!("resolving deps of {}", addr.format()))?;
    let td = &def.target_def;

    let mut deps = Vec::with_capacity(td.inputs.len());
    let mut seen = std::collections::HashSet::new();
    for input in &td.inputs {
        let a = &input.r#ref.r#ref;
        if seen.insert(a.clone()) {
            deps.push(a.clone());
        }
    }

    Ok(NodeInfo { deps })
}

/// Restores the terminal on drop, even if the render loop errors or panics.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        drop(disable_raw_mode());
        drop(execute!(io::stderr(), LeaveAlternateScreen, Show));
    }
}

pub(crate) async fn run(
    engine: Arc<Engine>,
    root: Addr,
    sink: LogSink,
    _shutdown: ShutdownTrigger,
) -> anyhow::Result<()> {
    // Suppress engine log lines for the lifetime of the explorer: in raw alt
    // screen a stray write would corrupt the frame. Held, never drained.
    let _log_rx = sink.switch_to_buffered();

    enable_raw_mode().context("enabling raw mode")?;
    execute!(io::stderr(), EnterAlternateScreen, Hide).context("entering alt screen")?;
    let _guard = TermGuard;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stderr())).context("building terminal")?;

    let mut events = EventStream::new();

    // Root load behind the spinner too — it can be slow (provider walk + parse).
    // A fetch error propagates via `?`: the TermGuard restores the terminal and
    // the error prints to stderr. A user quit during the load returns early.
    let root_fut: BoxFuture<anyhow::Result<NodeInfo>> = {
        let engine = Arc::clone(&engine);
        let root = root.clone();
        Box::pin(async move { fetch_node(&engine, &root).await })
    };
    let Some(root_info) =
        fetch_with_spinner(&mut terminal, &mut events, &root.format(), root_fut).await?
    else {
        return Ok(());
    };
    let mut explorer = Explorer::new(root.clone(), root_info);

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut spinner_idx = 0usize;

    // In-flight drill fetch: `(target, future)`. While `Some`, a spinner shows
    // and new drills are suppressed, but navigation and quit stay responsive.
    let mut pending: Option<(Addr, BoxFuture<anyhow::Result<NodeInfo>>)> = None;

    loop {
        let spinner = spinner_frame(spinner_idx);
        terminal.draw(|f| render(f, &mut explorer, spinner))?;

        let mut drill_in = false;
        tokio::select! {
            _ = ticker.tick() => {
                spinner_idx = spinner_idx.wrapping_add(1);
            }
            // Poll the in-flight fetch (only when one exists).
            res = poll_pending(&mut pending), if pending.is_some() => {
                let (dep, result) = res;
                explorer.loading = None;
                match result {
                    Ok(info) => explorer.drill(dep, info),
                    Err(e) => explorer.status = Some(format!("error: {e:#}")),
                }
            }
            ev = events.next() => {
                let Some(Ok(Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }))) = ev else {
                    if ev.is_none() { break; }
                    continue;
                };
                if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }
                if explorer.searching() {
                    match code {
                        KeyCode::Esc => explorer.cancel_search(),
                        KeyCode::Enter | KeyCode::Left => drill_in = true,
                        KeyCode::Up => explorer.up(),
                        KeyCode::Down => explorer.down(),
                        KeyCode::Backspace => explorer.search_backspace(),
                        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                            explorer.search_push(c)
                        }
                        _ => {}
                    }
                } else {
                    match code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('/') => explorer.start_search(),
                        KeyCode::Up => explorer.up(),
                        KeyCode::Down => explorer.down(),
                        KeyCode::Right | KeyCode::Esc => {
                            explorer.back();
                        }
                        KeyCode::Enter | KeyCode::Left => drill_in = true,
                        _ => {}
                    }
                }
            }
        }

        // Start a drill fetch in the background — unless one is already running.
        if drill_in
            && pending.is_none()
            && let Some(dep) = explorer.selected()
        {
            explorer.loading = Some(dep.clone());
            explorer.status = None;
            let fut: BoxFuture<anyhow::Result<NodeInfo>> = {
                let engine = Arc::clone(&engine);
                let dep = dep.clone();
                Box::pin(async move { fetch_node(&engine, &dep).await })
            };
            pending = Some((dep, fut));
        }
    }

    Ok(())
}

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// Await the in-flight drill fetch, returning its target and result. Parks
/// forever when nothing is pending (the `select!` arm is guarded so this only
/// runs when `pending` is `Some`).
async fn poll_pending(
    pending: &mut Option<(Addr, BoxFuture<anyhow::Result<NodeInfo>>)>,
) -> (Addr, anyhow::Result<NodeInfo>) {
    match pending {
        Some((addr, fut)) => {
            let addr = addr.clone();
            let result = fut.await;
            *pending = None;
            (addr, result)
        }
        None => std::future::pending().await,
    }
}

/// Spinner cadence and frames, matching the main progress renderer.
const TICK: std::time::Duration = std::time::Duration::from_millis(80);
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame(idx: usize) -> &'static str {
    SPINNER_FRAMES
        .get(idx % SPINNER_FRAMES.len())
        .copied()
        .unwrap_or("⠋")
}

/// Drive `fut` to completion while drawing a centered loading spinner, staying
/// responsive to `q` / `Esc` / Ctrl-C. Returns `Ok(None)` if the user quit
/// before it finished; propagates a fetch error via `?` at the call site.
async fn fetch_with_spinner(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    events: &mut EventStream,
    label: &str,
    mut fut: BoxFuture<anyhow::Result<NodeInfo>>,
) -> anyhow::Result<Option<NodeInfo>> {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idx = 0usize;
    loop {
        let spinner = spinner_frame(idx);
        terminal.draw(|f| {
            let area = f.area();
            let line = Line::from(vec![
                Span::styled(format!("{spinner} "), Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("loading {label}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]);
            f.render_widget(Paragraph::new(line).centered(), area);
        })?;

        tokio::select! {
            _ = ticker.tick() => idx = idx.wrapping_add(1),
            res = &mut fut => return Ok(Some(res?)),
            ev = events.next() => match ev {
                None => return Ok(None),
                Some(Ok(Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }))) => {
                    let ctrl_c = code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl_c || matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                        return Ok(None);
                    }
                }
                _ => {}
            },
        }
    }
}

/// Most trail rows the bottom panel grows to before it starts scrolling.
const MAX_TRAIL_ROWS: usize = 10;

fn render(f: &mut ratatui::Frame, explorer: &mut Explorer, spinner: &str) {
    let area = f.area();
    // The trail panel grows downward as the chain deepens (border + one row per
    // level, capped), so the deps list keeps the rest of the height.
    let trail_rows = explorer.path.len().min(MAX_TRAIL_ROWS) as u16;
    let trail_h = trail_rows + 2;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),          // active dep list
            Constraint::Length(trail_h), // accumulated drill trail
            Constraint::Length(1),       // help / status
        ])
        .split(area);

    let [list, trail, footer] = rows.as_ref() else {
        return;
    };
    render_dep_list(f, *list, explorer);
    render_trail(f, *trail, explorer);
    render_footer(f, *footer, explorer, spinner);
}

fn render_footer(f: &mut ratatui::Frame, area: Rect, explorer: &Explorer, spinner: &str) {
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let line = if let Some(addr) = &explorer.loading {
        Line::from(vec![
            Span::styled(format!("{spinner} "), Style::default().fg(Color::Yellow)),
            Span::styled(format!("loading {}", addr.format()), dim),
        ])
    } else if let Some(status) = &explorer.status {
        Line::from(Span::styled(
            status.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if let Some(s) = &explorer.search {
        Line::from(vec![
            Span::styled("/", Style::default().fg(Color::White)),
            Span::styled(
                s.query.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   ↵ open   esc cancel", dim),
        ])
    } else {
        Line::from(Span::styled(
            "↑/↓ select   ←/↵ drill in   →/esc back   / search   q quit",
            dim,
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// The accumulated drill trail at the bottom: one row per level, root at top →
/// current at bottom, indented to show depth. The current level is highlighted;
/// ancestors are dim. Full width, so the full address (with args) fits;
/// left-truncated only if it overflows.
fn render_trail(f: &mut ratatui::Frame, area: Rect, explorer: &Explorer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White))
        .title(" trail ");
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let last = explorer.path.len() - 1;

    let items: Vec<ListItem> = explorer
        .path
        .iter()
        .enumerate()
        .map(|(i, level)| {
            // Indent each level to render the chain as a descending staircase.
            let indent = "  ".repeat(i);
            let avail = inner_w.saturating_sub(indent.len());
            let text = format!("{indent}{}", truncate_left(&level.addr.format(), avail));
            let style = if i == last {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    // Scroll so the current (bottom) level stays visible on deep chains.
    let mut state = ListState::default();
    state.select(Some(last));
    if inner_h > 0 && last + 1 > inner_h {
        *state.offset_mut() = last + 1 - inner_h;
    }
    f.render_stateful_widget(List::new(items).block(block), area, &mut state);
}

fn render_dep_list(f: &mut ratatui::Frame, area: Rect, explorer: &mut Explorer) {
    // Indices into the current level's deps that are visible (all, or the search
    // matches). Cursor + scroll offset index into this visible list.
    let visible = explorer.visible();
    let cursor = explorer.cursor();

    // Keep the cursor inside the viewport: visible rows = inner height.
    let inner_h = area.height.saturating_sub(2) as usize;
    let voff = if let Some(s) = &mut explorer.search {
        clamp_offset(&mut s.voff, cursor, inner_h);
        s.voff
    } else {
        let cur = explorer.current_mut();
        clamp_offset(&mut cur.voff, cursor, inner_h);
        cur.voff
    };

    let total = explorer.current().info.deps.len();
    let title = match &explorer.search {
        Some(_) => format!(" deps ({}/{}) ", visible.len(), total),
        None => format!(" deps ({total}) "),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White))
        .title(title);

    if visible.is_empty() {
        let msg = if explorer.searching() {
            "(no matches)"
        } else {
            "(no deps)"
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )))
        .block(block);
        f.render_widget(p, area);
        return;
    }

    let cur = explorer.current();
    let items: Vec<ListItem> = visible
        .iter()
        .filter_map(|&i| cur.info.deps.get(i))
        .map(|a| {
            let marker = if explorer.on_path(a) { " ↺" } else { "" };
            ListItem::new(format!("{}{}", a.format(), marker))
        })
        .collect();

    // Solid white bar with black text — high contrast against the dim theme so
    // the highlighted row is unmistakable.
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::White)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = ListState::default();
    state.select(Some(cursor));
    *state.offset_mut() = voff;
    f.render_stateful_widget(list, area, &mut state);
}

/// Scroll `off` so the row at `cursor` stays within `inner_h` visible rows.
fn clamp_offset(off: &mut usize, cursor: usize, inner_h: usize) {
    if inner_h == 0 {
        return;
    }
    if cursor < *off {
        *off = cursor;
    } else if cursor >= *off + inner_h {
        *off = cursor + 1 - inner_h;
    }
}

/// Truncate `s` to at most `max` chars, dropping from the *left* and prefixing
/// `…` so the trailing path segments and `:name` always stay visible.
fn truncate_left(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let tail = max - 1; // reserve 1 char for the ellipsis
    let tail_str: String = s.chars().skip(len - tail).collect();
    format!("…{tail_str}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htpkg::PkgBuf;
    use std::collections::BTreeMap;

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("pkg"), name.to_string(), BTreeMap::new())
    }

    fn info(deps: &[&str]) -> NodeInfo {
        NodeInfo {
            deps: deps.iter().map(|d| addr(d)).collect(),
        }
    }

    #[test]
    fn down_and_up_clamp_at_bounds() {
        let mut e = Explorer::new(addr("root"), info(&["a", "b"]));
        assert_eq!(e.selected(), Some(addr("a")));
        e.down();
        assert_eq!(e.selected(), Some(addr("b")));
        // clamps at last
        e.down();
        assert_eq!(e.selected(), Some(addr("b")));
        e.up();
        assert_eq!(e.selected(), Some(addr("a")));
        // clamps at first
        e.up();
        assert_eq!(e.selected(), Some(addr("a")));
    }

    #[test]
    fn drill_pushes_level_and_resets_selection() {
        let mut e = Explorer::new(addr("root"), info(&["a", "b"]));
        e.down(); // select b
        let dep = e.selected().expect("selection");
        e.drill(dep.clone(), info(&["c"]));
        assert_eq!(e.path.len(), 2);
        // current is now `b`, its deps shown, selection reset to first
        assert_eq!(e.current().addr, addr("b"));
        assert_eq!(e.selected(), Some(addr("c")));
    }

    #[test]
    fn back_pops_and_preserves_parent_selection() {
        let mut e = Explorer::new(addr("root"), info(&["a", "b"]));
        e.down(); // select b in root
        e.drill(addr("b"), info(&["c"]));
        assert!(e.back());
        // back at root with the original selection intact
        assert_eq!(e.current().addr, addr("root"));
        assert_eq!(e.selected(), Some(addr("b")));
    }

    #[test]
    fn back_at_root_is_noop() {
        let mut e = Explorer::new(addr("root"), info(&["a"]));
        assert!(!e.back());
        assert_eq!(e.path.len(), 1);
    }

    #[test]
    fn selected_is_none_when_no_deps() {
        let e = Explorer::new(addr("root"), info(&[]));
        assert_eq!(e.selected(), None);
    }

    #[test]
    fn on_path_detects_cycle() {
        let mut e = Explorer::new(addr("root"), info(&["a"]));
        e.drill(addr("a"), info(&["root"]));
        assert!(e.on_path(&addr("root")));
        assert!(!e.on_path(&addr("zzz")));
    }

    #[test]
    fn search_filters_and_selects_match() {
        let mut e = Explorer::new(addr("root"), info(&["alpha", "beta", "gamma"]));
        e.start_search();
        "a".chars().for_each(|c| e.search_push(c));
        // "a" matches all three (alphA, betA, gammA)
        assert_eq!(e.visible().len(), 3);
        e.search_push('l'); // "al" -> only alpha
        assert_eq!(e.visible().len(), 1);
        assert_eq!(e.selected(), Some(addr("alpha")));
    }

    #[test]
    fn search_down_navigates_filtered_set() {
        let mut e = Explorer::new(addr("root"), info(&["alpha", "beta", "bravo"]));
        e.start_search();
        e.search_push('b'); // beta, bravo
        assert_eq!(e.selected(), Some(addr("beta")));
        e.down();
        assert_eq!(e.selected(), Some(addr("bravo")));
        e.down(); // clamps within filtered set
        assert_eq!(e.selected(), Some(addr("bravo")));
    }

    #[test]
    fn drill_clears_active_search() {
        let mut e = Explorer::new(addr("root"), info(&["alpha", "beta"]));
        e.start_search();
        e.search_push('b');
        let dep = e.selected().expect("match");
        e.drill(dep, info(&["c"]));
        assert!(!e.searching());
        // new level shows its full dep list
        assert_eq!(e.selected(), Some(addr("c")));
    }

    #[test]
    fn truncate_left_keeps_tail() {
        assert_eq!(truncate_left("//short:x", 20), "//short:x");
        let t = truncate_left("//go/large/store/delta:pkg", 14);
        assert_eq!(t.chars().count(), 14);
        assert!(t.starts_with('…'));
        assert!(t.ends_with(":pkg"));
    }

    #[test]
    fn cancel_search_restores_full_list() {
        let mut e = Explorer::new(addr("root"), info(&["alpha", "beta"]));
        e.start_search();
        e.search_push('z'); // no matches
        assert!(e.visible().is_empty());
        e.cancel_search();
        assert_eq!(e.visible().len(), 2);
        assert_eq!(e.selected(), Some(addr("alpha")));
    }
}
