//! Drag to select on a full screen; the selection goes to the clipboard when
//! the button is released.
//!
//! A full screen captures the mouse (its clicks and wheel are the
//! dashboard's), which turns the terminal's own selection off there. This
//! puts it back. A left press freezes the frame on screen; a drag highlights
//! the cells from the press to the pointer in reading order, the way a
//! terminal selects; the release copies their text (the backend writes it
//! as an OSC 52). A press released without the pointer ever going further
//! than the next cell on its row is a click, so a click's jitter neither
//! swallows the click nor overwrites the clipboard.
//!
//! The frame stays frozen until the release, so what is copied is what was
//! highlighted: the view keeps folding events, but nothing it draws reaches
//! the screen while the button is down. Both come from one walk over the
//! same cells ([`cells`]), so they cannot disagree at a wide character.
//!
//! A press in a scrolling pane (the approval view's notice, a target
//! screen's tab body) selects in that pane's whole text, the [`SelectDoc`]
//! the view hands over at the press, rather than in the rows on screen. The
//! ends of the selection are rows of the doc, so the drag scrolls the pane:
//! a wheel step scrolls it under the held button, and the pointer held past
//! its top or bottom edge scrolls it, faster the further past: a step on
//! each render tick while the pointer is still, and on each motion report
//! at most every [`AUTOSCROLL`]. Rows selected and scrolled away stay selected and are
//! copied. A selection's rows keep to the pane, from its text column (past
//! line numbers or an indent) to its right edge; a selection that runs to
//! the right edge takes the rest of a row the pane cut short, and a wrapped
//! line is copied as one line. The release leaves the pane where the drag
//! scrolled it.
//!
//! Every way a drag ends without a release — a key, a resize, a new press,
//! or [`IDLE`] with no mouse report (a release the terminal never sent) —
//! copies nothing and gives the screen back to the view, scrolled as it
//! was. So does a wheel step on a screen with no pane under the press.

use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;

use crate::tui::app::SelectDoc;

/// How long a drag may go without a mouse report (or a scroll step) before
/// it is taken for a release the terminal never sent: the frame is live
/// again, nothing copied.
pub(super) const IDLE: Duration = Duration::from_secs(10);

/// The least time between two steps of the scroll a pointer held past a
/// pane's edge drives. A step moves one row per row the pointer is past it.
pub(super) const AUTOSCROLL: Duration = Duration::from_millis(50);

/// A selection's last column on a row: to the row's end, past the screen
/// edge when the pane cut it there.
const END: u16 = u16::MAX;

/// The mouse state of a full screen: at most one press held.
#[derive(Debug, Default)]
pub(super) struct Selection {
    drag: Option<Drag>,
}

/// What a mouse report asks of the backend.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// Nothing to do.
    Nothing,
    /// The highlight changed: draw the frozen frame with it.
    Repaint,
    /// A click where the button went down, delivered at the release.
    Click(Position),
    /// The selected text, one line per row (a wrapped line one line), each
    /// without its trailing blanks.
    Copy(String),
}

/// What a release asks of the backend.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Released {
    pub action: Action,
    /// The row of the doc the drag left at the top of its pane, when it
    /// scrolled it: the view scrolls there (see
    /// [`TUIAppView::selection_scrolled`](crate::tui::app::TUIAppView::selection_scrolled)).
    pub scrolled: Option<usize>,
}

impl Selection {
    /// A left press at `at` on the frame `frozen`. In `doc`'s pane when the
    /// press is in its text, else on the frame as drawn. Replaces a press
    /// still held (its release was lost).
    pub(super) fn press(
        &mut self,
        frozen: Buffer,
        doc: Option<SelectDoc>,
        at: Position,
        now: Instant,
    ) {
        let pane = doc
            .and_then(|d| Pane::new(d, frozen.area))
            .filter(|p| p.bounds.contains(at));
        let bounds = pane.as_ref().map_or(frozen.area, |p| p.bounds);
        let press = clamp(frozen.area, at);
        let mut d = Drag {
            frozen,
            pane,
            bounds,
            press,
            pointer: at,
            anchor: Spot::default(),
            head: Spot::default(),
            moved: false,
            seen: now,
            stepped: None,
        };
        d.anchor = d.locate(at);
        d.head = d.anchor;
        self.drag = Some(d);
    }

    /// The pointer is at `at` with the button down.
    pub(super) fn motion(&mut self, at: Position, now: Instant) -> Action {
        let Some(d) = self.drag.as_mut() else {
            return Action::Nothing;
        };
        d.seen = now;
        let changed = d.point(at);
        let scrolled = d.step(now);
        // A jitter moves the head but shows no selection.
        if (changed || scrolled) && d.moved {
            Action::Repaint
        } else {
            Action::Nothing
        }
    }

    /// A render tick: a pointer held past its pane's edge scrolls it on.
    /// Whether it did, and the frame wants drawing.
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        self.drag.as_mut().is_some_and(|d| d.step(now))
    }

    /// A wheel step of `rows` (negative up) while the button is down. In a
    /// pane it scrolls the pane and the selection follows the pointer
    /// through it. `None` when the step is not the drag's: no press held,
    /// or one on a screen with no pane, which it ends. The view takes it.
    pub(super) fn wheel(&mut self, rows: isize, now: Instant) -> Option<Action> {
        let d = self.drag.as_mut()?;
        if d.pane.is_none() {
            self.drag = None;
            return None;
        }
        d.seen = now;
        Some(if d.scroll(rows) {
            Action::Repaint
        } else {
            Action::Nothing
        })
    }

    /// The button came up at `at`. Nothing when no press is held: it went
    /// down before the screen was up, or something ended the drag.
    pub(super) fn release(&mut self, at: Position) -> Released {
        let Some(mut d) = self.drag.take() else {
            return Released {
                action: Action::Nothing,
                scrolled: None,
            };
        };
        d.point(at);
        let scrolled = d
            .pane
            .as_ref()
            .filter(|p| p.doc.top != p.top0)
            .map(|p| p.doc.top);
        let action = if d.moved {
            let text = d.text();
            if text.trim().is_empty() {
                Action::Nothing
            } else {
                Action::Copy(text)
            }
        } else {
            Action::Click(d.press)
        };
        Released { action, scrolled }
    }

    /// A key or a resize: the drag ends, nothing copied, the pane back where
    /// it was. Whether one was held.
    pub(super) fn cancel(&mut self) -> bool {
        self.drag.take().is_some()
    }

    /// End a drag that has had no mouse report nor scroll step for
    /// [`IDLE`]. Whether one ended.
    pub(super) fn expire(&mut self, now: Instant) -> bool {
        let stale = self
            .drag
            .as_ref()
            .is_some_and(|d| now.saturating_duration_since(d.seen) >= IDLE);
        stale && self.cancel()
    }

    /// The press held, if any.
    pub(super) fn held(&self) -> Option<&Drag> {
        self.drag.as_ref()
    }
}

/// A place in what a drag selects in: a row (of the frame, or of the pane's
/// doc) and a screen column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Spot {
    row: usize,
    col: u16,
}

/// The pane a press landed in: its doc, scrolled to `doc.top`.
#[derive(Debug)]
struct Pane {
    /// Its area cut to the frame, and its top kept in its rows.
    doc: SelectDoc,
    /// The top the view drew it at.
    top0: usize,
    /// The cells a selection takes: the area from the text column on.
    bounds: Rect,
}

impl Pane {
    /// `doc` on a frame `frame`; `None` when nothing of it is on the frame,
    /// or it has no rows.
    fn new(mut doc: SelectDoc, frame: Rect) -> Option<Self> {
        let area = doc.area.intersection(frame);
        if area.is_empty() || doc.rows.is_empty() {
            return None;
        }
        doc.area = area;
        let left = doc
            .text_left
            .clamp(area.left(), area.right().saturating_sub(1));
        let bounds = Rect::new(left, area.y, area.right() - left, area.height);
        let mut pane = Self {
            top0: 0,
            doc,
            bounds,
        };
        pane.doc.top = pane.doc.top.min(pane.max_top());
        pane.top0 = pane.doc.top;
        Some(pane)
    }

    /// The furthest down the pane scrolls: its last row at the bottom.
    fn max_top(&self) -> usize {
        self.doc
            .rows
            .len()
            .saturating_sub(usize::from(self.doc.area.height))
    }

    /// The screen row doc row `row` is on, when it is on screen.
    fn screen_row(&self, row: usize) -> Option<u16> {
        let k = u16::try_from(row.checked_sub(self.doc.top)?).ok()?;
        (k < self.doc.area.height).then(|| self.doc.area.y + k)
    }
}

/// A left press held on a full screen: the frame it froze and what it
/// spans so far.
#[derive(Debug)]
pub(super) struct Drag {
    frozen: Buffer,
    /// The scrolling pane the press was in, if any.
    pane: Option<Pane>,
    /// The cells a selection takes: the pane's text, or the whole frame.
    bounds: Rect,
    /// Where the button went down, on the frame.
    press: Position,
    /// Where the pointer was last reported, past the edges or not.
    pointer: Position,
    anchor: Spot,
    head: Spot,
    /// The pointer went further than the next cell on the press's row at
    /// some point, or the pane scrolled: the release copies rather than
    /// clicks, even back there.
    moved: bool,
    /// The last mouse report or scroll step, for [`IDLE`].
    seen: Instant,
    /// The last step of the scroll past the pane's edge.
    stepped: Option<Instant>,
}

impl Drag {
    /// Whether the frozen frame is the size of `area`: after a resize it is
    /// not, and the drag is over.
    pub(super) fn fits(&self, area: Rect) -> bool {
        self.frozen.area == area
    }

    /// Draw the frozen frame into `buf` (the same size, see [`Self::fits`]),
    /// the pane redrawn from its doc once the drag has scrolled it, with the
    /// selected cells in reverse video: toggled, as a terminal inverts, so
    /// a cell the view drew reversed shows plain.
    pub(super) fn paint(&self, buf: &mut Buffer) {
        // `clone_from` on the cells reuses `buf`'s allocation.
        buf.content.clone_from(&self.frozen.content);
        if let Some(p) = self.pane.as_ref().filter(|p| p.doc.top != p.top0) {
            let a = p.doc.area;
            for (y, row) in (a.top()..a.bottom()).zip(p.doc.top..) {
                for x in a.left()..a.right() {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.reset();
                    }
                }
                if let Some(r) = p.doc.rows.get(row) {
                    buf.set_line(a.x, y, &r.line, a.width);
                }
            }
        }
        if !self.moved {
            return;
        }
        let right = self.bounds.right().saturating_sub(1);
        for (row, from, to) in self.rows() {
            let y = match &self.pane {
                Some(p) => p.screen_row(row),
                None => u16::try_from(row).ok(),
            };
            let Some(y) = y else {
                continue;
            };
            let spans: Vec<(u16, u16)> = cells(buf, y, from, to.min(right))
                .map(|(col, width, _)| (col, width))
                .collect();
            for (col, width) in spans {
                for c in col..col.saturating_add(width) {
                    if let Some(cell) = buf.cell_mut((c, y)) {
                        cell.modifier.toggle(Modifier::REVERSED);
                    }
                }
            }
        }
    }

    /// Where the pointer at `at` puts the selection's head. In a pane, past
    /// its top is the start of the top row, past its bottom (or past the
    /// doc's last row) the end of the row there.
    fn locate(&self, at: Position) -> Spot {
        let b = self.bounds;
        let right = b.right().saturating_sub(1).max(b.left());
        let col = at.x.clamp(b.left(), right);
        let Some(p) = &self.pane else {
            let y =
                at.y.clamp(b.top(), b.bottom().saturating_sub(1).max(b.top()));
            return Spot {
                row: usize::from(y),
                col,
            };
        };
        let last = p.doc.rows.len().saturating_sub(1);
        if at.y < b.top() {
            return Spot {
                row: p.doc.top,
                col: b.left(),
            };
        }
        let k = if at.y >= b.bottom() {
            b.height.saturating_sub(1)
        } else {
            at.y - b.top()
        };
        let row = p.doc.top + usize::from(k);
        if row > last {
            Spot {
                row: last,
                col: right,
            }
        } else if at.y >= b.bottom() {
            Spot { row, col: right }
        } else {
            Spot { row, col }
        }
    }

    /// The pointer is at `at`. Whether the head moved.
    fn point(&mut self, at: Position) -> bool {
        self.pointer = at;
        let on_frame = clamp(self.frozen.area, at);
        self.moved |= on_frame.y != self.press.y || on_frame.x.abs_diff(self.press.x) > 1;
        let head = self.locate(at);
        if head == self.head {
            return false;
        }
        self.head = head;
        true
    }

    /// One step of the scroll a pointer past the pane's top or bottom
    /// drives, when [`AUTOSCROLL`] has passed since the last. Whether the
    /// pane moved.
    fn step(&mut self, now: Instant) -> bool {
        let Some(p) = &self.pane else {
            return false;
        };
        let (b, y) = (p.bounds, self.pointer.y);
        let rows = if y < b.top() {
            -isize::from(i16::try_from(b.top() - y).unwrap_or(i16::MAX))
        } else if y >= b.bottom() {
            isize::from(i16::try_from(y - b.bottom()).unwrap_or(i16::MAX - 1)) + 1
        } else {
            return false;
        };
        if self
            .stepped
            .is_some_and(|at| now.saturating_duration_since(at) < AUTOSCROLL)
        {
            return false;
        }
        self.stepped = Some(now);
        let moved = self.scroll(rows);
        if moved {
            self.seen = now;
        }
        moved
    }

    /// Scroll the pane `rows` (negative up), within its doc, the head kept
    /// under the pointer. Whether it moved.
    fn scroll(&mut self, rows: isize) -> bool {
        let Some(p) = self.pane.as_mut() else {
            return false;
        };
        let top = p.doc.top.saturating_add_signed(rows).min(p.max_top());
        if top == p.doc.top {
            return false;
        }
        p.doc.top = top;
        self.moved = true;
        self.head = self.locate(self.pointer);
        true
    }

    /// The selected text: each row's cells, rows joined by `\n` (a wrapped
    /// row by nothing), each line without its trailing blanks (a wrapped
    /// line's, across all its rows).
    fn text(&self) -> String {
        let mut out = String::new();
        let mut joined = false;
        let mut first = true;
        let right = self.bounds.right().saturating_sub(1);
        let end_row = self.ends().1.row;
        // Where the line being copied starts: a wrapped line's rows are one.
        let mut start = 0;
        for (row, from, to) in self.rows() {
            if !joined {
                if !first {
                    out.push('\n');
                }
                start = out.len();
            }
            first = false;
            match &self.pane {
                None => {
                    if let Ok(y) = u16::try_from(row) {
                        for (_, _, symbol) in cells(&self.frozen, y, from, to.min(right)) {
                            out.push_str(symbol);
                        }
                    }
                    joined = false;
                }
                Some(p) => {
                    let Some(r) = p.doc.rows.get(row) else {
                        continue;
                    };
                    let line = r.whole.as_ref().unwrap_or(&r.line);
                    // The row alone, at least the pane's width: a whole row
                    // runs past the screen's edge.
                    let x = p.doc.area.x;
                    let width = u16::try_from(line.width())
                        .unwrap_or(u16::MAX)
                        .max(p.doc.area.width)
                        .min(u16::MAX - x);
                    let mut buf = Buffer::empty(Rect::new(x, 0, width, 1));
                    let (drawn_to, _) = buf.set_line(x, 0, line, width);
                    // Only the row's own cells: a wrapped row's blanks at
                    // its end are text, the padding past it is not.
                    let to = to.min(drawn_to.saturating_sub(1));
                    if drawn_to > from {
                        for (_, _, symbol) in cells(&buf, 0, from, to) {
                            out.push_str(symbol);
                        }
                    }
                    joined = r.joined && row != end_row;
                }
            }
            if !joined {
                let kept = out
                    .get(start..)
                    .map_or(0, |line| line.trim_end_matches(' ').len());
                out.truncate(start + kept);
            }
        }
        out
    }

    /// The selection's two ends in reading order.
    fn ends(&self) -> (Spot, Spot) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Each selected row with its first and last selected column, top to
    /// bottom: from the earlier end to the later one in reading order, every
    /// row in between across the whole of [`Self::bounds`]. In a pane, an
    /// end at its right edge is [`END`]: the rest of the row.
    fn rows(&self) -> impl Iterator<Item = (usize, u16, u16)> + '_ {
        let (start, end) = self.ends();
        let left = self.bounds.left();
        let right = self.bounds.right().saturating_sub(1);
        let end_col = if self.pane.is_some() && end.col >= right {
            END
        } else {
            end.col
        };
        (start.row..=end.row).map(move |row| {
            let from = if row == start.row { start.col } else { left };
            let to = if row == end.row { end_col } else { END };
            (row, from, to)
        })
    }
}

/// The cells of `buf`'s row `y` that meet columns `from..=to`, as (first
/// column, width, symbol). A wide character half inside is there whole.
/// Widths are ratatui's own ([`CellWidth`]), the ones the frame was laid
/// out with.
fn cells(buf: &Buffer, y: u16, from: u16, to: u16) -> impl Iterator<Item = (u16, u16, &str)> + '_ {
    let mut col = buf.area.left();
    std::iter::from_fn(move || {
        while col <= to {
            let cell = buf.cell((col, y))?;
            let symbol = cell.symbol();
            let width = symbol.cell_width().max(1);
            let at = col;
            col = col.saturating_add(width);
            if at.saturating_add(width) > from {
                return Some((at, width, symbol));
            }
        }
        None
    })
}

/// `at` moved inside `area`: a report from past the edge (a drag out of the
/// window) selects up to it.
fn clamp(area: Rect, at: Position) -> Position {
    Position::new(
        at.x.clamp(area.left(), area.right().saturating_sub(1).max(area.left())),
        at.y.clamp(area.top(), area.bottom().saturating_sub(1).max(area.top())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::DocRow;
    use ratatui::style::Style;
    use ratatui::text::Line;

    /// A frozen frame holding `lines`, `width` wide.
    fn frame(width: u16, lines: &[&str]) -> Buffer {
        let height = u16::try_from(lines.len()).expect("few lines");
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        for (y, line) in (0u16..).zip(lines) {
            buf.set_string(0, y, line, Style::default());
        }
        buf
    }

    fn p(x: u16, y: u16) -> Position {
        Position::new(x, y)
    }

    /// Press at `from`, move to `to`, release there.
    fn drag(buf: Buffer, from: Position, to: Position) -> Action {
        drag_in(buf, None, from, to)
    }

    fn drag_in(buf: Buffer, doc: Option<SelectDoc>, from: Position, to: Position) -> Action {
        let now = Instant::now();
        let mut s = Selection::default();
        s.press(buf, doc, from, now);
        s.motion(to, now);
        s.release(to).action
    }

    /// The cells `paint` reverses, in reading order.
    fn reversed(buf: &Buffer) -> Vec<(u16, u16)> {
        let a = buf.area;
        (a.top()..a.bottom())
            .flat_map(|y| (a.left()..a.right()).map(move |x| (x, y)))
            .filter(|&pos| {
                buf.cell(pos)
                    .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
            })
            .collect()
    }

    /// The selection held after a press at `from` and a move to `to`,
    /// painted onto a frame the view drew since.
    fn painted(frozen: Buffer, from: Position, to: Position) -> Buffer {
        let now = Instant::now();
        let mut s = Selection::default();
        let mut buf = Buffer::empty(frozen.area);
        s.press(frozen, None, from, now);
        s.motion(to, now);
        s.held().expect("held").paint(&mut buf);
        buf
    }

    /// The text of `buf`'s row `y`, trailing blanks dropped.
    fn row_text(buf: &Buffer, y: u16) -> String {
        let a = buf.area;
        let s: String = (a.left()..a.right())
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
            .collect();
        s.trim_end().to_owned()
    }

    #[test]
    fn a_press_released_in_place_is_a_click() {
        assert_eq!(
            drag(frame(10, &["hello"]), p(2, 0), p(2, 0)),
            Action::Click(p(2, 0))
        );
    }

    #[test]
    fn a_one_cell_jitter_is_still_a_click_at_the_press() {
        let buf = frame(10, &["hello"]);
        assert_eq!(drag(buf, p(2, 0), p(3, 0)), Action::Click(p(2, 0)));
        let buf = frame(10, &["hello"]);
        assert_eq!(drag(buf, p(2, 0), p(1, 0)), Action::Click(p(2, 0)));
    }

    #[test]
    fn a_jitter_paints_no_selection() {
        let buf = painted(frame(10, &["hello"]), p(2, 0), p(3, 0));
        assert!(reversed(&buf).is_empty());
        assert_eq!(
            buf.cell((1, 0)).map(|c| c.symbol().to_owned()).as_deref(),
            Some("e")
        );
    }

    #[test]
    fn a_one_row_move_is_a_drag() {
        let buf = frame(10, &["ab", "cd"]);
        assert_eq!(drag(buf, p(0, 0), p(0, 1)), Action::Copy("ab\nc".into()));
    }

    #[test]
    fn a_drag_back_to_its_start_copies_that_cell() {
        let now = Instant::now();
        let mut s = Selection::default();
        s.press(frame(10, &["hello"]), None, p(1, 0), now);
        assert_eq!(s.motion(p(4, 0), now), Action::Repaint);
        assert_eq!(s.release(p(1, 0)).action, Action::Copy("e".into()));
    }

    #[test]
    fn a_drag_on_one_row_copies_the_cells_between_both_ends() {
        let buf = frame(20, &["the quick fox"]);
        assert_eq!(drag(buf, p(4, 0), p(8, 0)), Action::Copy("quick".into()));
    }

    #[test]
    fn a_drag_backwards_copies_the_same_cells() {
        let buf = frame(20, &["the quick fox"]);
        assert_eq!(drag(buf, p(8, 0), p(4, 0)), Action::Copy("quick".into()));
    }

    #[test]
    fn rows_in_between_are_whole_and_trailing_blanks_dropped() {
        let buf = frame(12, &["first line", "middle", "last line"]);
        assert_eq!(
            drag(buf, p(6, 0), p(3, 2)),
            Action::Copy("line\nmiddle\nlast".into())
        );
    }

    #[test]
    fn an_upward_drag_reads_top_to_bottom() {
        let buf = frame(12, &["first line", "middle", "last line"]);
        assert_eq!(
            drag(buf, p(3, 2), p(6, 0)),
            Action::Copy("line\nmiddle\nlast".into())
        );
    }

    #[test]
    fn a_blank_row_in_between_stays_an_empty_line() {
        let buf = frame(8, &["ab", "", "cd"]);
        assert_eq!(drag(buf, p(0, 0), p(1, 2)), Action::Copy("ab\n\ncd".into()));
    }

    #[test]
    fn blanks_inside_a_row_are_kept() {
        let buf = frame(12, &["a    b"]);
        assert_eq!(drag(buf, p(0, 0), p(9, 0)), Action::Copy("a    b".into()));
    }

    #[test]
    fn a_drag_over_blanks_copies_nothing() {
        let buf = frame(10, &["ab"]);
        assert_eq!(drag(buf, p(4, 0), p(8, 0)), Action::Nothing);
    }

    #[test]
    fn a_wide_character_is_taken_once_and_whole() {
        // `漢` covers columns 1 and 2; the selection starts on its second
        // half and ends on the first half of `字`.
        let buf = frame(10, &["a漢b字c"]);
        assert_eq!(drag(buf, p(2, 0), p(4, 0)), Action::Copy("漢b字".into()));
    }

    #[test]
    fn a_wide_character_is_highlighted_whole_as_it_is_copied() {
        let buf = painted(frame(10, &["a漢b字c"]), p(2, 0), p(4, 0));
        assert_eq!(reversed(&buf), [(1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]);
    }

    #[test]
    fn widths_are_the_ones_the_frame_was_laid_out_with() {
        // A halfwidth katakana with its sound mark is one grapheme ratatui
        // lays out three cells wide, where `unicode-width` says two.
        let buf = frame(10, &["\u{ff76}\u{ff9e}X"]);
        assert_eq!(
            drag(buf, p(0, 0), p(5, 0)),
            Action::Copy("\u{ff76}\u{ff9e}X".into())
        );
    }

    #[test]
    fn a_report_past_the_edge_selects_to_it() {
        let buf = frame(5, &["abcde", "fghij"]);
        assert_eq!(
            drag(buf, p(3, 0), p(200, 200)),
            Action::Copy("de\nfghij".into())
        );
    }

    #[test]
    fn paint_reverses_the_selection_on_the_frozen_frame() {
        let buf = painted(frame(6, &["abcdef", "ghijkl"]), p(4, 0), p(1, 1));
        let symbol = |col: u16, row: u16| buf.cell((col, row)).map(|c| c.symbol().to_owned());
        assert_eq!(symbol(0, 0).as_deref(), Some("a"));
        assert_eq!(symbol(5, 1).as_deref(), Some("l"));
        assert_eq!(reversed(&buf), [(4, 0), (5, 0), (0, 1), (1, 1)]);
    }

    #[test]
    fn a_cell_drawn_reversed_shows_plain_under_the_selection() {
        let mut frozen = frame(4, &["abcd"]);
        if let Some(c) = frozen.cell_mut((1, 0)) {
            c.modifier.insert(Modifier::REVERSED);
        }
        let buf = painted(frozen, p(0, 0), p(3, 0));
        assert_eq!(reversed(&buf), [(0, 0), (2, 0), (3, 0)]);
    }

    #[test]
    fn a_resize_ends_the_fit() {
        let mut s = Selection::default();
        s.press(frame(4, &["abcd"]), None, p(1, 0), Instant::now());
        let d = s.held().expect("held");
        assert!(d.fits(Rect::new(0, 0, 4, 1)));
        assert!(!d.fits(Rect::new(0, 0, 5, 1)));
    }

    #[test]
    fn a_zero_size_frame_selects_nothing() {
        let buf = Buffer::empty(Rect::new(0, 0, 0, 0));
        assert_eq!(drag(buf, p(0, 0), p(3, 3)), Action::Click(p(0, 0)));
    }

    // A pane: a header row, then rows 1..=3 showing a doc of numbered lines
    // (`NN text`, the text from column 3), then a hint row.

    const PANE: Rect = Rect::new(0, 1, 12, 3);
    const HINT_ROW: u16 = 4;

    fn doc_line(k: usize) -> String {
        format!("{k:>2} line{k}")
    }

    /// `n` numbered rows, the pane scrolled to `top`.
    fn numbered(n: usize, top: usize) -> SelectDoc {
        SelectDoc {
            area: PANE,
            text_left: 3,
            rows: (0..n)
                .map(|k| DocRow::plain(Line::raw(doc_line(k))))
                .collect(),
            top,
        }
    }

    /// The frame the view drew for `doc`: the header, its rows from
    /// `doc.top`, the hints.
    fn pane_frame(doc: &SelectDoc) -> Buffer {
        let mut lines = vec!["header".to_owned()];
        for k in 0..usize::from(PANE.height) {
            lines.push(doc.rows.get(doc.top + k).map_or(String::new(), |r| {
                r.line.spans.iter().map(|s| s.content.as_ref()).collect()
            }));
        }
        lines.push("hints".to_owned());
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        frame(PANE.width, &refs)
    }

    fn pressed(doc: SelectDoc, at: Position, now: Instant) -> Selection {
        let mut s = Selection::default();
        s.press(pane_frame(&doc), Some(doc), at, now);
        s
    }

    fn copied(r: Released) -> String {
        match r.action {
            Action::Copy(text) => text,
            // Fails the comparison it is in, saying what it was.
            other => format!("not a copy: {other:?}"),
        }
    }

    #[test]
    fn a_press_in_a_pane_selects_only_its_text() {
        let doc = numbered(3, 0);
        // From `ne0` to past the bottom: every row from its text column.
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(5, 1), p(9, HINT_ROW)),
            Action::Copy("ne0\nline1\nline2".into())
        );
    }

    #[test]
    fn a_press_outside_the_pane_selects_the_frame_as_drawn() {
        let doc = numbered(3, 0);
        // On the gutter: not the pane's text.
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(0, 0), p(3, 1)),
            Action::Copy("header\n 0 l".into())
        );
    }

    #[test]
    fn a_wheel_step_scrolls_the_pane_under_the_selection() {
        let now = Instant::now();
        let mut s = pressed(numbered(10, 0), p(3, 1), now);
        assert_eq!(s.motion(p(7, 2), now), Action::Repaint);
        assert_eq!(s.wheel(3, now), Some(Action::Repaint));
        // The pointer is on row 2 of the pane, now doc row 4.
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
        s.held().expect("held").paint(&mut buf);
        assert_eq!(row_text(&buf, 0), "header", "outside the pane, frozen");
        assert_eq!(row_text(&buf, 1), doc_line(3));
        assert_eq!(row_text(&buf, 3), doc_line(5));
        assert_eq!(row_text(&buf, HINT_ROW), "hints");
        // Doc rows 3 and 4 are on screen and selected; 0..=2 are above.
        assert_eq!(
            reversed(&buf),
            (3..12)
                .map(|x| (x, 1))
                .chain((3..8).map(|x| (x, 2)))
                .collect::<Vec<_>>()
        );
        let r = s.release(p(7, 2));
        assert_eq!(r.scrolled, Some(3));
        assert_eq!(copied(r), "line0\nline1\nline2\nline3\nline4");
    }

    #[test]
    fn a_wheel_step_back_to_where_it_started_reports_no_scroll() {
        let now = Instant::now();
        let mut s = pressed(numbered(10, 2), p(3, 1), now);
        s.motion(p(7, 1), now);
        s.wheel(3, now);
        s.wheel(-3, now);
        let r = s.release(p(7, 1));
        assert_eq!(r.scrolled, None);
        assert_eq!(copied(r), "line2");
    }

    #[test]
    fn a_scroll_under_a_still_pointer_makes_it_a_selection() {
        let now = Instant::now();
        let mut s = pressed(numbered(10, 0), p(3, 1), now);
        assert_eq!(s.wheel(1, now), Some(Action::Repaint));
        let r = s.release(p(3, 1));
        assert_eq!(r.scrolled, Some(1));
        assert_eq!(copied(r), "line0\nl");
    }

    #[test]
    fn the_wheel_stops_at_the_docs_ends() {
        let now = Instant::now();
        let mut s = pressed(numbered(5, 0), p(3, 1), now);
        assert_eq!(s.wheel(-3, now), Some(Action::Nothing));
        assert_eq!(s.wheel(30, now), Some(Action::Repaint));
        assert_eq!(s.wheel(1, now), Some(Action::Nothing));
        assert_eq!(s.release(p(3, 1)).scrolled, Some(2), "5 rows, 3 shown");
    }

    #[test]
    fn a_wheel_step_with_no_pane_ends_the_drag_and_is_the_views() {
        let now = Instant::now();
        let mut s = Selection::default();
        s.press(frame(8, &["hello"]), None, p(0, 0), now);
        assert_eq!(s.wheel(3, now), None);
        assert!(s.held().is_none());
        assert_eq!(s.wheel(3, now), None, "nothing held");
    }

    #[test]
    fn a_pointer_held_below_the_pane_scrolls_it_each_step() {
        let t0 = Instant::now();
        let mut s = pressed(numbered(20, 0), p(3, 1), t0);
        // On the hint row: one row past the bottom.
        assert_eq!(s.motion(p(5, HINT_ROW), t0), Action::Repaint);
        assert_eq!(
            s.held().and_then(|d| d.pane.as_ref()).map(|p| p.doc.top),
            Some(1)
        );
        assert!(!s.tick(t0 + AUTOSCROLL / 2), "not before a step's time");
        assert!(s.tick(t0 + AUTOSCROLL));
        assert!(s.tick(t0 + AUTOSCROLL * 2));
        let r = s.release(p(5, HINT_ROW));
        assert_eq!(r.scrolled, Some(3));
        // Down to the end of the row at the bottom: doc row 5.
        assert_eq!(copied(r), "line0\nline1\nline2\nline3\nline4\nline5");
    }

    #[test]
    fn further_past_the_edge_scrolls_faster() {
        let t0 = Instant::now();
        let mut s = pressed(numbered(40, 0), p(3, 1), t0);
        s.motion(p(5, HINT_ROW + 3), t0);
        assert_eq!(
            s.held().and_then(|d| d.pane.as_ref()).map(|p| p.doc.top),
            Some(4),
            "four rows past the bottom, four rows a step"
        );
    }

    #[test]
    fn a_pointer_above_the_pane_scrolls_up_to_the_start_of_the_top_row() {
        let t0 = Instant::now();
        let mut s = pressed(numbered(20, 5), p(8, 3), t0);
        s.motion(p(9, 0), t0);
        assert!(s.tick(t0 + AUTOSCROLL));
        let r = s.release(p(9, 0));
        assert_eq!(r.scrolled, Some(3));
        assert_eq!(copied(r), "line3\nline4\nline5\nline6\nline7");
    }

    #[test]
    fn the_scroll_stops_at_the_end_of_the_doc() {
        let t0 = Instant::now();
        let mut s = pressed(numbered(4, 0), p(3, 1), t0);
        s.motion(p(3, HINT_ROW), t0);
        assert!(!s.tick(t0 + AUTOSCROLL), "4 rows, 3 shown: at the end");
        assert_eq!(
            copied(s.release(p(3, HINT_ROW))),
            "line0\nline1\nline2\nline3"
        );
    }

    #[test]
    fn scroll_steps_keep_a_drag_alive_past_idle() {
        let t0 = Instant::now();
        let mut s = pressed(numbered(400, 0), p(3, 1), t0);
        s.motion(p(3, HINT_ROW), t0);
        let mut now = t0;
        while now < t0 + IDLE + AUTOSCROLL {
            now += AUTOSCROLL;
            s.tick(now);
            assert!(!s.expire(now), "a drag still scrolling expired");
        }
        // Still held, still scrolling: a step a row, 400 rows.
        let top = s.held().and_then(|d| d.pane.as_ref()).map(|p| p.doc.top);
        assert!(top.is_some_and(|t| t > 200 && t < 397), "{top:?}");
    }

    #[test]
    fn past_the_docs_last_row_is_its_end() {
        let doc = numbered(2, 0);
        // Rows 1–2 show the doc, row 3 is blank pane.
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(3, 1), p(4, 3)),
            Action::Copy("line0\nline1".into())
        );
    }

    #[test]
    fn a_wrapped_line_is_copied_as_one_line() {
        let row = |text: &str, joined: bool| DocRow {
            line: Line::raw(format!("   {text}")),
            whole: None,
            joined,
        };
        // `hello world` wrapped at 9 columns, the space ending the first row.
        let doc = SelectDoc {
            area: PANE,
            text_left: 3,
            rows: vec![row("hello wor", true), row("ld", false), row("next", false)],
            top: 0,
        };
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(3, 1), p(11, 3)),
            Action::Copy("hello world\nnext".into())
        );
        let doc = SelectDoc {
            area: PANE,
            text_left: 3,
            rows: vec![row("hello ", true), row("world", false)],
            top: 0,
        };
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(3, 1), p(11, 2)),
            Action::Copy("hello world".into()),
            "the blank a wrap ends a row on is text"
        );
    }

    #[test]
    fn a_wrapped_lines_trailing_blanks_go_across_its_rows() {
        let row = |text: &str, joined: bool| DocRow {
            line: Line::raw(format!("   {text}")),
            whole: None,
            joined,
        };
        // `abc` and eleven blanks, wrapped at 9 columns.
        let doc = SelectDoc {
            area: PANE,
            text_left: 3,
            rows: vec![row("abc      ", true), row("     ", false), row("x", false)],
            top: 0,
        };
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(3, 1), p(11, 3)),
            Action::Copy("abc\nx".into())
        );
    }

    #[test]
    fn a_wide_character_in_a_doc_row_is_copied_whole() {
        let doc = SelectDoc {
            area: PANE,
            text_left: 3,
            rows: vec![DocRow::plain(Line::raw(" 1 a漢b"))],
            top: 0,
        };
        // From the second half of `漢` to the end of the row.
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(5, 1), p(11, 1)),
            Action::Copy("漢b".into())
        );
    }

    #[test]
    fn a_selection_to_the_right_edge_takes_the_rest_of_a_cut_row() {
        let cut = |text: &str, whole: &str| DocRow {
            line: Line::raw(format!("   {text}")),
            whole: Some(Line::raw(format!("   {whole}"))),
            joined: false,
        };
        let rows = || {
            vec![
                cut("abcdefgh…", "abcdefghijklmnop"),
                cut("qrstuvwx…", "qrstuvwxyz0123"),
            ]
        };
        let doc = SelectDoc {
            area: PANE,
            text_left: 3,
            rows: rows(),
            top: 0,
        };
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(4, 1), p(11, 2)),
            Action::Copy("bcdefghijklmnop\nqrstuvwxyz0123".into())
        );
        let doc = SelectDoc {
            area: PANE,
            text_left: 3,
            rows: rows(),
            top: 0,
        };
        assert_eq!(
            drag_in(pane_frame(&doc), Some(doc), p(4, 1), p(5, 2)),
            Action::Copy("bcdefghijklmnop\nqrs".into()),
            "short of the edge: only the cells selected"
        );
    }

    #[test]
    fn a_cancel_leaves_the_pane_where_the_view_had_it() {
        let now = Instant::now();
        let mut s = pressed(numbered(10, 0), p(3, 1), now);
        s.wheel(3, now);
        assert!(s.cancel());
        assert_eq!(
            s.release(p(3, 1)),
            Released {
                action: Action::Nothing,
                scrolled: None
            }
        );
    }

    /// Each way a drag can go, as the backend feeds it: what every step asks.
    #[test]
    fn event_sequences() {
        #[derive(Clone, Copy)]
        enum Ev {
            Press(u16, u16),
            Motion(u16, u16),
            Release(u16, u16),
            /// A key or a resize.
            Cancel,
            /// A wheel step on a screen with no pane.
            Wheel,
            /// Time passes with no mouse report.
            Wait(Duration),
        }
        use Action::{Click, Copy, Nothing, Repaint};
        use Ev::{Cancel, Motion, Press, Release, Wait, Wheel};
        let copy = |s: &str| Copy(s.to_owned());
        let cases: Vec<(&str, Vec<Ev>, Vec<Action>)> = vec![
            (
                "press, drag, release",
                vec![Press(0, 0), Motion(4, 0), Release(4, 0)],
                vec![Nothing, Repaint, copy("hello")],
            ),
            (
                "a motion into the same cell repaints nothing",
                vec![Press(0, 0), Motion(4, 0), Motion(4, 0), Release(4, 0)],
                vec![Nothing, Repaint, Nothing, copy("hello")],
            ),
            (
                "press, key, release: no click, no copy",
                vec![Press(0, 0), Motion(4, 0), Cancel, Release(4, 0)],
                vec![Nothing, Repaint, Nothing, Nothing],
            ),
            (
                "press, wheel, release in place: no click",
                vec![Press(1, 0), Wheel, Release(1, 0)],
                vec![Nothing, Nothing, Nothing],
            ),
            (
                "a release with no press",
                vec![Release(2, 0)],
                vec![Nothing],
            ),
            (
                "a motion with no press",
                vec![Motion(2, 0), Release(2, 0)],
                vec![Nothing, Nothing],
            ),
            (
                "a lost release: the next press starts over",
                vec![Press(0, 0), Motion(4, 0), Press(0, 1), Release(0, 1)],
                vec![Nothing, Repaint, Nothing, Click(p(0, 1))],
            ),
            (
                "a lost release: idle ends the drag",
                vec![Press(0, 0), Motion(4, 0), Wait(IDLE), Release(4, 0)],
                vec![Nothing, Repaint, Nothing, Nothing],
            ),
            (
                "motion keeps a drag alive past idle",
                vec![
                    Press(0, 0),
                    Wait(IDLE / 2),
                    Motion(4, 0),
                    Wait(IDLE / 2),
                    Release(4, 0),
                ],
                vec![Nothing, Nothing, Repaint, Nothing, copy("hello")],
            ),
        ];
        for (name, events, want) in cases {
            let mut s = Selection::default();
            let mut now = Instant::now();
            let got: Vec<Action> = events
                .iter()
                .map(|&ev| match ev {
                    Press(x, y) => {
                        s.press(frame(8, &["hello", "world"]), None, p(x, y), now);
                        Nothing
                    }
                    Motion(x, y) => s.motion(p(x, y), now),
                    Release(x, y) => s.release(p(x, y)).action,
                    Cancel => {
                        s.cancel();
                        Nothing
                    }
                    Wheel => s.wheel(3, now).unwrap_or(Nothing),
                    Wait(d) => {
                        now += d;
                        s.expire(now);
                        Nothing
                    }
                })
                .collect();
            assert_eq!(got, want, "{name}");
        }
    }
}
