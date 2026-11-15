//! The full-screen header: a title row, a row the gauges move down to, the
//! tab row and the rule under it. Four rows at every width, never wrapped;
//! what narrows is what goes in them, in the spec's order.

use hcore::capacity::Unit;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::model::{Gauge, Tab, TabCounts};
use crate::tui::text::{clean_inline, cut_right, take_width, width};

/// What the title row says.
#[derive(Debug, Clone)]
pub struct Title<'a> {
    pub command: &'a str,
    pub session: Option<&'a str>,
    pub clock: &'a str,
    /// Done and cached: the line's done figure.
    pub done: usize,
    /// The matched total; `None` before it is known.
    pub total: Option<usize>,
    /// The matcher still streams: the total is `~`-prefixed.
    pub provisional: bool,
    /// `auto-approving (Y)` / `auto-rejecting (N)`; never dropped.
    pub standing: Option<&'a str>,
}

/// The steps the header took, for the narrowing test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Levels {
    /// 0 full, 1 no session id, 2 short done count, 3 command cut.
    pub title: u8,
    /// The gauges took the row under the title.
    pub gauges_down: bool,
    /// 0 cells, 1 numbers only, 2 + folded pools.
    pub gauges: u8,
    /// 0 gaps of four, 1 gaps of two, 2 inactive tabs without names, 3 the
    /// active tab alone.
    pub tabs: u8,
}

const GAUGE_CELLS: usize = 4;
const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
const GAUGE_GAP: &str = "   ";

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// Spans cut to `cols` columns, never inside a character.
fn fit(spans: Vec<Span<'static>>, cols: usize) -> Line<'static> {
    let mut left = cols;
    let mut out = Vec::with_capacity(spans.len());
    for s in spans {
        if left == 0 {
            break;
        }
        let w = width(&s.content);
        if w <= left {
            left -= w;
            out.push(s);
        } else {
            let cut = take_width(&s.content, left).to_owned();
            left = 0;
            out.push(Span::styled(cut, s.style));
        }
    }
    Line::from(out)
}

fn label(pool: &str) -> &str {
    if pool == hcore::capacity::MEMORY {
        "mem"
    } else {
        pool
    }
}

/// `held/capacity` in the pool's unit, short: `11/12`, `9.1/16G`.
fn amounts(g: &Gauge) -> String {
    match g.unit {
        Unit::Bytes => {
            let (div, suffix) = [
                (1u64 << 40, "T"),
                (1 << 30, "G"),
                (1 << 20, "M"),
                (1 << 10, "K"),
            ]
            .into_iter()
            .find(|&(d, _)| g.capacity >= d)
            .unwrap_or((1, "B"));
            let short = |v: u64| {
                let tenths = (v.saturating_mul(10) + div / 2) / div;
                if tenths.is_multiple_of(10) {
                    format!("{}", tenths / 10)
                } else {
                    format!("{}.{}", tenths / 10, tenths % 10)
                }
            };
            format!("{}/{}{suffix}", short(g.held), short(g.capacity))
        }
        unit => format!(
            "{}/{}",
            hcore::events::display_amount(unit, g.held),
            hcore::events::display_amount(unit, g.capacity)
        ),
    }
}

/// Four cells at eighth-block precision: how much of the pool is held.
fn cells(g: &Gauge) -> String {
    let eighths = if g.capacity == 0 {
        0
    } else {
        let total = (GAUGE_CELLS * 8) as u128;
        let e = (u128::from(g.held) * total + u128::from(g.capacity) / 2) / u128::from(g.capacity);
        usize::try_from(e.min(total)).unwrap_or(GAUGE_CELLS * 8)
    };
    let full = eighths / 8;
    let mut s = "█".repeat(full);
    if full < GAUGE_CELLS {
        s.push_str(PARTIAL.get(eighths % 8).copied().unwrap_or(""));
        let drawn = full + usize::from(eighths % 8 != 0);
        s.push_str(&"░".repeat(GAUGE_CELLS - drawn));
    }
    s
}

fn gauge_spans(g: &Gauge, with_cells: bool) -> Vec<Span<'static>> {
    let style = if g.waiting > 0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let name = clean_inline(label(&g.pool)).into_owned();
    let mut text = if with_cells {
        format!("{name} {} {}", cells(g), amounts(g))
    } else {
        format!("{name} {}", amounts(g))
    };
    if g.waiting > 0 {
        text.push_str(&format!(" ⧗{}", g.waiting));
    }
    vec![Span::styled(text, style)]
}

/// The gauges, cells or numbers, with `folded` of them (by index) shown as
/// `+N pools`.
fn gauges_row(gauges: &[&Gauge], with_cells: bool, folded: &[bool]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut n_folded = 0;
    for (i, g) in gauges.iter().enumerate() {
        if folded.get(i).copied().unwrap_or(false) {
            n_folded += 1;
            continue;
        }
        if !out.is_empty() {
            out.push(Span::raw(GAUGE_GAP));
        }
        out.extend(gauge_spans(g, with_cells));
    }
    if n_folded > 0 {
        if !out.is_empty() {
            out.push(Span::raw(GAUGE_GAP));
        }
        let noun = if n_folded == 1 { "pool" } else { "pools" };
        out.push(Span::styled(
            format!("+{n_folded} {noun}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    out
}

/// The order pools fold in: from the right, a pool with waiters never, cpu
/// last.
fn fold_order(gauges: &[&Gauge]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..gauges.len())
        .rev()
        .filter(|&i| {
            gauges
                .get(i)
                .is_some_and(|g| g.waiting == 0 && g.pool != hcore::capacity::CPU)
        })
        .collect();
    order.extend((0..gauges.len()).filter(|&i| {
        gauges
            .get(i)
            .is_some_and(|g| g.waiting == 0 && g.pool == hcore::capacity::CPU)
    }));
    order
}

fn title_spans(t: &Title<'_>, level: u8, command_cols: Option<usize>) -> Vec<Span<'static>> {
    let command = clean_inline(t.command);
    let command = match command_cols {
        Some(c) => cut_right(&command, c).into_owned(),
        None => command.into_owned(),
    };
    let mut out = Vec::new();
    if !command.is_empty() {
        out.push(Span::styled(
            command,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    if level < 1
        && let Some(id) = t.session
    {
        out.push(Span::styled(
            format!("  session {}", clean_inline(id)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let tilde = if t.provisional { "~" } else { "" };
    let done = match (t.total, level < 2) {
        (Some(total), true) => format!("{} / {tilde}{total} done", t.done),
        (Some(total), false) => format!("{}/{tilde}{total}", t.done),
        (None, _) => format!("{} done", t.done),
    };
    let lead = if out.is_empty() { "" } else { "   " };
    out.push(Span::raw(format!("{lead}{} · {done}", t.clock)));
    if let Some(s) = t.standing {
        out.push(Span::styled(
            format!("   {}", clean_inline(s)),
            Style::default().fg(Color::Yellow),
        ));
    }
    out
}

/// The tab row at `level`, and the active tab's columns for the rule.
fn tabs_row(
    counts: &TabCounts,
    active: Tab,
    level: u8,
) -> (Vec<Span<'static>>, std::ops::Range<usize>) {
    let mut out = vec![Span::raw("  ")];
    let mut at = 2;
    let mut range = 0..0;
    let gap = if level == 0 { "    " } else { "  " };
    let tab_text = |tab: Tab, named: bool| -> String {
        let n = counts.tab(tab);
        match (tab.glyph(), named) {
            (None, _) => format!("{} {n}", tab.name()),
            (Some(g), true) => format!("{g} {} {n}", tab.name()),
            (Some(g), false) => format!("{g} {n}"),
        }
    };
    let style = |tab: Tab| {
        let n = counts.tab(tab);
        let base = if n == 0 {
            Style::default().fg(Color::DarkGray)
        } else {
            tab.style()
        };
        if tab == active {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    if level >= 3 {
        let text = format!("‹ {} ›", tab_text(active, true));
        range = at..at + width(&text);
        out.push(Span::styled(text, style(active)));
        return (out, range);
    }
    for (i, &tab) in Tab::LIST.iter().enumerate() {
        if i > 0 {
            out.push(Span::raw(gap));
            at += gap.len();
        }
        let text = tab_text(tab, level < 2 || tab == active);
        let w = width(&text);
        if tab == active {
            range = at..at + w;
        }
        at += w;
        out.push(Span::styled(text, style(tab)));
    }
    (out, range)
}

fn rule(cols: usize, active: std::ops::Range<usize>) -> Line<'static> {
    let before = active.start.min(cols);
    let under = active.end.min(cols) - before;
    let after = cols - before - under;
    Line::from(vec![
        Span::styled("─".repeat(before), Style::default().fg(Color::DarkGray)),
        Span::styled(
            "━".repeat(under),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(after), Style::default().fg(Color::DarkGray)),
    ])
}

/// The header's four rows at `cols` columns.
pub fn header_lines(
    title: &Title<'_>,
    gauges: &[&Gauge],
    counts: &TabCounts,
    active: Tab,
    cols: u16,
) -> ([Line<'static>; 4], Levels) {
    let cols = usize::from(cols);
    // The title and gauge rows start one column in.
    let inner = cols.saturating_sub(1);
    let mut lv = Levels::default();

    // Title row: full with the gauges beside it while both fit.
    let full = title_spans(title, 0, None);
    let cells_row = gauges_row(gauges, true, &[]);
    let side = !gauges.is_empty()
        && spans_width(&full) + GAUGE_GAP.len() + spans_width(&cells_row) <= inner;
    let mut title_row = if side {
        let mut row = full;
        // Right-align the gauges.
        let gap = inner - spans_width(&row) - spans_width(&cells_row);
        row.push(Span::raw(" ".repeat(gap)));
        row.extend(cells_row);
        row
    } else {
        lv.gauges_down = !gauges.is_empty();
        let mut chosen = None;
        for level in 0..3u8 {
            let row = title_spans(title, level, None);
            if spans_width(&row) <= inner {
                lv.title = level;
                chosen = Some(row);
                break;
            }
        }
        chosen.unwrap_or_else(|| {
            lv.title = 3;
            // What the command may keep: the rest, with the clock whole.
            let rest = spans_width(&title_spans(title, 2, Some(0)));
            let room = inner.saturating_sub(rest + 3);
            title_spans(title, 2, Some(room))
        })
    };
    if title_row.is_empty() {
        title_row.push(Span::raw(""));
    }

    // Gauge row.
    let gauge_row = if lv.gauges_down {
        let numbers = gauges_row(gauges, false, &[]);
        let cells_row = gauges_row(gauges, true, &[]);
        if spans_width(&cells_row) <= inner {
            cells_row
        } else if spans_width(&numbers) <= inner {
            lv.gauges = 1;
            numbers
        } else {
            lv.gauges = 2;
            let mut folded = vec![false; gauges.len()];
            let mut row = numbers;
            for i in fold_order(gauges) {
                if spans_width(&row) <= inner {
                    break;
                }
                if let Some(f) = folded.get_mut(i) {
                    *f = true;
                }
                row = gauges_row(gauges, false, &folded);
            }
            row
        }
    } else {
        Vec::new()
    };

    // Tab row.
    let mut tabs = (Vec::new(), 0..0);
    for level in 0..=3u8 {
        tabs = tabs_row(counts, active, level);
        lv.tabs = level;
        if spans_width(&tabs.0) <= cols {
            break;
        }
    }

    let lead = |mut spans: Vec<Span<'static>>| {
        spans.insert(0, Span::raw(" "));
        spans
    };
    let rule = rule(cols, (tabs.1.start)..(tabs.1.end));
    (
        [
            fit(lead(title_row), cols),
            fit(lead(gauge_row), cols),
            fit(tabs.0, cols),
            rule,
        ],
        lv,
    )
}
