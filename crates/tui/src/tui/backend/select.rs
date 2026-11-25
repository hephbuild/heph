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
//! The frame stays frozen until the release, so what is copied is exactly
//! what was highlighted: the view keeps folding events, but nothing it draws
//! reaches the screen while the button is down. Both come from one walk over
//! the frozen cells ([`Drag::spans`]), so they cannot disagree at a wide
//! character.
//!
//! A press can be confined to a region of the screen (the Log tab's text,
//! without its line numbers): rows of the selection then run from the
//! region's left edge to its right, never across the rest of the frame.
//!
//! Every way a drag ends without a release — a key, a wheel step, a resize,
//! a new press, or [`IDLE`] with no mouse report (a release the terminal
//! never sent) — copies nothing and gives the screen back to the view.

use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;

/// How long a drag may go without a mouse report before it is taken for a
/// release the terminal never sent: the frame is live again, nothing copied.
pub(super) const IDLE: Duration = Duration::from_secs(10);

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
    /// The selected text, one line per row, each without its trailing blanks.
    Copy(String),
}

impl Selection {
    /// A left press at `at` on the frame `frozen`, confined to `region` when
    /// the press is inside it. Replaces a press still held (its release was
    /// lost).
    pub(super) fn press(
        &mut self,
        frozen: Buffer,
        region: Option<Rect>,
        at: Position,
        now: Instant,
    ) {
        let bounds = region
            .map(|r| r.intersection(frozen.area))
            .filter(|r| !r.is_empty() && r.contains(at))
            .unwrap_or(frozen.area);
        let at = clamp(bounds, at);
        self.drag = Some(Drag {
            frozen,
            bounds,
            anchor: at,
            head: at,
            moved: false,
            seen: now,
        });
    }

    /// The pointer is at `at` with the button down.
    pub(super) fn motion(&mut self, at: Position, now: Instant) -> Action {
        let Some(d) = self.drag.as_mut() else {
            return Action::Nothing;
        };
        d.seen = now;
        // A jitter moves the head but shows no selection.
        if d.extend(at) && d.moved {
            Action::Repaint
        } else {
            Action::Nothing
        }
    }

    /// The button came up at `at`. Nothing when no press is held: it went
    /// down before the screen was up, or something ended the drag.
    pub(super) fn release(&mut self, at: Position) -> Action {
        let Some(mut d) = self.drag.take() else {
            return Action::Nothing;
        };
        d.extend(at);
        if !d.moved {
            return Action::Click(d.anchor);
        }
        let text = d.text();
        if text.trim().is_empty() {
            Action::Nothing
        } else {
            Action::Copy(text)
        }
    }

    /// A key, a wheel step or a resize: the drag ends, nothing copied.
    /// Whether one was held.
    pub(super) fn cancel(&mut self) -> bool {
        self.drag.take().is_some()
    }

    /// End a drag that has had no mouse report for [`IDLE`]. Whether one
    /// ended.
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

/// A left press held on a full screen: the frame it froze and the cells it
/// spans so far.
#[derive(Debug)]
pub(super) struct Drag {
    frozen: Buffer,
    /// Where the selection may run: the region the press was in, or the
    /// whole frame.
    bounds: Rect,
    anchor: Position,
    head: Position,
    /// The pointer went further than the next cell on the press's row at
    /// some point: the release copies rather than clicks, even back there.
    moved: bool,
    /// The last mouse report, for [`IDLE`].
    seen: Instant,
}

impl Drag {
    /// Whether the frozen frame is the size of `area`: after a resize it is
    /// not, and the drag is over.
    pub(super) fn fits(&self, area: Rect) -> bool {
        self.frozen.area == area
    }

    /// Draw the frozen frame into `buf` (the same size, see [`Self::fits`]),
    /// with the selected cells in reverse video: toggled, as a terminal
    /// inverts, so a cell the view drew reversed shows plain.
    pub(super) fn paint(&self, buf: &mut Buffer) {
        // `clone_from` on the cells reuses `buf`'s allocation.
        buf.content.clone_from(&self.frozen.content);
        if !self.moved {
            return;
        }
        for (row, from, to) in self.rows() {
            for (col, width, _) in self.spans(row, from, to) {
                for c in col..col.saturating_add(width) {
                    if let Some(cell) = buf.cell_mut((c, row)) {
                        cell.modifier.toggle(Modifier::REVERSED);
                    }
                }
            }
        }
    }

    /// The pointer is at `at`. Whether the head moved.
    fn extend(&mut self, at: Position) -> bool {
        let at = clamp(self.bounds, at);
        if at == self.head {
            return false;
        }
        self.head = at;
        self.moved |= at.y != self.anchor.y || at.x.abs_diff(self.anchor.x) > 1;
        true
    }

    /// The selected text: each row's spans, trailing blanks dropped, rows
    /// joined by `\n`.
    fn text(&self) -> String {
        let mut out = String::new();
        for (i, (row, from, to)) in self.rows().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let start = out.len();
            for (_, _, symbol) in self.spans(row, from, to) {
                out.push_str(symbol);
            }
            let kept = out
                .get(start..)
                .map_or(0, |line| line.trim_end_matches(' ').len());
            out.truncate(start + kept);
        }
        out
    }

    /// The cells of `row` that meet columns `from..=to`, as (first column,
    /// width, symbol). A wide character half inside is there whole. Widths
    /// are ratatui's own ([`CellWidth`]), the ones the frame was laid out
    /// with.
    fn spans(&self, row: u16, from: u16, to: u16) -> impl Iterator<Item = (u16, u16, &str)> + '_ {
        let mut col = self.frozen.area.left();
        std::iter::from_fn(move || {
            while col <= to {
                let cell = self.frozen.cell((col, row))?;
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

    /// Each selected row with its first and last selected column, top to
    /// bottom: from the earlier end to the later one in reading order, every
    /// row in between across the whole of [`Self::bounds`].
    fn rows(&self) -> impl Iterator<Item = (u16, u16, u16)> + '_ {
        let (a, h) = (self.anchor, self.head);
        let (start, end) = if (a.y, a.x) <= (h.y, h.x) {
            (a, h)
        } else {
            (h, a)
        };
        let (left, right) = (self.bounds.left(), self.bounds.right().saturating_sub(1));
        (start.y..=end.y).map(move |row| {
            let from = if row == start.y { start.x } else { left };
            let to = if row == end.y { end.x } else { right };
            (row, from, to)
        })
    }
}

/// `at` moved inside `area`: a report from past the edge (a drag out of the
/// window, or out of the region) selects up to it.
fn clamp(area: Rect, at: Position) -> Position {
    Position::new(
        at.x.clamp(area.left(), area.right().saturating_sub(1).max(area.left())),
        at.y.clamp(area.top(), area.bottom().saturating_sub(1).max(area.top())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

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

    fn drag_in(buf: Buffer, region: Option<Rect>, from: Position, to: Position) -> Action {
        let now = Instant::now();
        let mut s = Selection::default();
        s.press(buf, region, from, now);
        s.motion(to, now);
        s.release(to)
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
        assert_eq!(s.release(p(1, 0)), Action::Copy("e".into()));
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
    fn a_press_in_a_region_selects_only_inside_it() {
        // A gutter of two columns, then the text from column 3, rows 1–2.
        let buf = frame(10, &["header", " 1 alpha", " 2 beta", "hints"]);
        let region = Some(Rect::new(3, 1, 7, 2));
        assert_eq!(
            drag_in(buf, region, p(5, 1), p(9, 3)),
            Action::Copy("pha\nbeta".into())
        );
    }

    #[test]
    fn a_press_outside_the_region_selects_the_whole_frame() {
        let buf = frame(10, &["header", " 1 alpha"]);
        let region = Some(Rect::new(3, 1, 7, 1));
        assert_eq!(
            drag_in(buf, region, p(0, 0), p(3, 1)),
            Action::Copy("header\n 1 a".into())
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

    /// Each way a drag can go, as the backend feeds it: what every step asks.
    #[test]
    fn event_sequences() {
        #[derive(Clone, Copy)]
        enum Ev {
            Press(u16, u16),
            Motion(u16, u16),
            Release(u16, u16),
            /// A key, a wheel step or a resize.
            Cancel,
            /// Time passes with no mouse report.
            Wait(Duration),
        }
        use Action::{Click, Copy, Nothing, Repaint};
        use Ev::{Cancel, Motion, Press, Release, Wait};
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
                vec![Press(1, 0), Cancel, Release(1, 0)],
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
                    Release(x, y) => s.release(p(x, y)),
                    Cancel => {
                        s.cancel();
                        Nothing
                    }
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
