//! What every full screen draws around its body: the target and approval
//! screens' tab row and its rule, and the hint row each screen ends with.

use std::borrow::Cow;
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::text::{clean_inline, cut_right, width};

/// The breadcrumb that starts a sub-screen's tab row; a click on it goes
/// back.
pub(super) const BACK: &str = " ◂ esc  ";
/// Rows a screen keeps besides its body: the dashboard's four of header, or
/// a sub-screen's two of header, its tab row and rule; then a blank and the
/// hint row.
pub(super) const CHROME_ROWS: u16 = 6;
/// The tab row's place in the frame: the third row on every screen.
pub(super) const TABS_ROW: u16 = 2;

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Whether a click at `col` of a sub-screen's tab row hit the breadcrumb.
pub(super) fn on_back(col: u16) -> bool {
    usize::from(col) < width(BACK.trim_end())
}

/// A sub-screen's tab row, ` ◂ esc  <crumb>  │  <tab>    <tab>      <status>`,
/// and the rule under it, heavy under tab `active`. `hit` is told where each
/// tab's label is, by its index, for a click. The status is drawn in
/// `status_style` while there is room for it.
pub(super) fn sub_tab_rows<S: Into<Cow<'static, str>>>(
    crumb: impl IntoIterator<Item = Span<'static>>,
    tabs: impl IntoIterator<Item = S>,
    active: usize,
    (status, status_style): (&str, Style),
    cols: usize,
    mut hit: impl FnMut(usize, Range<u16>),
) -> (Line<'static>, Line<'static>) {
    let mut spans = vec![Span::styled(BACK, dim())];
    spans.extend(crumb);
    spans.push(Span::styled("  │  ", dim()));
    let mut at: usize = spans.iter().map(|s| width(&s.content)).sum();
    let mut mark = (0, 0);
    for (i, name) in tabs.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("    "));
            at += 4;
        }
        let name: Cow<'static, str> = name.into();
        let w = width(&name);
        let col = |c: usize| u16::try_from(c.min(cols)).unwrap_or(u16::MAX);
        hit(i, col(at)..col(at + w));
        if i == active {
            mark = (at, w);
            spans.push(Span::styled(
                name,
                Style::default().add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(name, dim()));
        }
        at += w;
    }
    let room = cols.saturating_sub(at + 2);
    if !status.is_empty() && room > 4 {
        let status = cut_right(status, room).into_owned();
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
    (line, Line::styled(rule, dim()))
}

/// The hint row: `text` on one line, cut to `cols` two columns in.
pub(crate) fn hint_line(text: &str, cols: usize, style: Style) -> Line<'static> {
    Line::styled(
        format!(
            "  {}",
            cut_right(&clean_inline(text), cols.saturating_sub(2))
        ),
        style,
    )
}

/// Ends a frame of `rows` rows: a blank, then `hint`. A frame already
/// taller than `rows` keeps its top and the hint row last.
pub(super) fn end_frame(out: &mut Vec<Line<'static>>, hint: Line<'static>, rows: u16) {
    out.push(Line::default());
    out.push(hint);
    let rows = usize::from(rows);
    if out.len() > rows {
        let hint = out.pop();
        out.truncate(rows.saturating_sub(1));
        out.extend(hint.filter(|_| rows > 0));
    }
}

/// [`end_frame`] for a sub-screen, whose body may be short: blank rows
/// first, so the hint row is the last of `rows`.
pub(super) fn end_sub_frame(out: &mut Vec<Line<'static>>, hint: Line<'static>, rows: u16) {
    let above = usize::from(rows.saturating_sub(2));
    if out.len() < above {
        out.resize(above, Line::default());
    }
    end_frame(out, hint, rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line<'_>) -> String {
        l.to_string()
    }

    #[test]
    fn end_frame_keeps_the_hint_row_last_when_short() {
        let hint = || hint_line("keys", 20, dim());
        for rows in 0..6u16 {
            let mut out = vec![Line::raw("a"), Line::raw("b"), Line::raw("c")];
            end_frame(&mut out, hint(), rows);
            let want = usize::from(rows).min(5);
            assert_eq!(out.len(), want, "rows {rows}");
            if rows > 0 {
                assert_eq!(
                    out.last().map(text).as_deref(),
                    Some("  keys"),
                    "rows {rows}"
                );
            }
        }
    }

    #[test]
    fn end_frame_does_not_pad_and_end_sub_frame_does() {
        let mut out = vec![Line::raw("a")];
        end_frame(&mut out, hint_line("keys", 20, dim()), 10);
        assert_eq!(out.len(), 3);
        let mut out = vec![Line::raw("a")];
        end_sub_frame(&mut out, hint_line("keys", 20, dim()), 10);
        assert_eq!(out.len(), 10);
        assert_eq!(out.last().map(text).as_deref(), Some("  keys"));
    }

    #[test]
    fn sub_tab_rows_reports_each_tab_and_marks_the_active_one() {
        let mut hits = Vec::new();
        let (row, rule) = sub_tab_rows(
            [Span::raw("//p:a")],
            ["One", "Two"],
            1,
            ("status", dim()),
            40,
            |i, r| hits.push((i, r)),
        );
        assert_eq!(text(&row), " ◂ esc  //p:a  │  One    Two     status");
        assert_eq!(hits, vec![(0, 18..21), (1, 25..28)]);
        assert_eq!(text(&rule).chars().filter(|&c| c == '━').count(), 3);
        assert!(text(&rule).chars().skip(25).take(3).all(|c| c == '━'));
        // No room for the status: the row never runs past `cols`.
        let (row, _) = sub_tab_rows(
            [Span::raw("//p:a")],
            ["One"],
            0,
            ("status", dim()),
            12,
            |_, _| {},
        );
        assert!(row.width() <= 12, "{row:?}");
    }
}
