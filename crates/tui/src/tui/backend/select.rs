//! Drag to select on a full screen; the selection goes to the clipboard when
//! the button is released.
//!
//! A full screen captures the mouse (its clicks and wheel are the
//! dashboard's), which turns the terminal's own selection off there. This
//! puts it back. A left press freezes the frame on screen; a drag highlights
//! the cells from the press to the pointer in reading order, the way a
//! terminal selects; the release copies their text (see
//! [`super::interactive`], which writes it as an OSC 52). A press released
//! where it went down, with no drag in between, is a click.
//!
//! The frame stays frozen until the release, so what is copied is exactly
//! what was highlighted: the view keeps folding events, but nothing it draws
//! reaches the screen while the button is down.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

/// A cell of the screen, `(column, row)`, 0-based, as a mouse report gives it.
pub(super) type At = (u16, u16);

/// A left press held on a full screen: the frame it froze and the cells it
/// spans so far.
#[derive(Debug)]
pub(super) struct Drag {
    frozen: Buffer,
    anchor: At,
    head: At,
    /// The pointer left the cell it was pressed on at some point: the
    /// release copies rather than clicks, even back on that cell.
    moved: bool,
}

/// What a release means.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Release {
    /// No drag: a click where the button went down.
    Click { col: u16, row: u16 },
    /// The selected text, one line per row, each without its trailing blanks.
    Copy(String),
    /// A drag over blanks only. Nothing is copied, so the clipboard keeps
    /// what it had.
    Nothing,
}

impl Drag {
    /// A press at `at` on the frame `frozen`.
    pub(super) fn new(frozen: Buffer, at: At) -> Self {
        let at = clamp(frozen.area, at);
        Self {
            frozen,
            anchor: at,
            head: at,
            moved: false,
        }
    }

    /// Whether the frozen frame is the size of `area`: after a resize it is
    /// not, and the drag is over.
    pub(super) fn fits(&self, area: Rect) -> bool {
        self.frozen.area == area
    }

    /// The pointer is at `at` with the button down. Whether the highlight
    /// changed.
    pub(super) fn extend(&mut self, at: At) -> bool {
        let at = clamp(self.frozen.area, at);
        if at == self.head {
            return false;
        }
        self.head = at;
        self.moved = true;
        true
    }

    /// The button came up at `at`.
    pub(super) fn release(mut self, at: At) -> Release {
        self.extend(at);
        if !self.moved {
            let (col, row) = self.anchor;
            return Release::Click { col, row };
        }
        let text = self.text();
        if text.trim().is_empty() {
            Release::Nothing
        } else {
            Release::Copy(text)
        }
    }

    /// Draw the frozen frame into `buf` (the same size, see [`Self::fits`]),
    /// with the selected cells in reverse video.
    pub(super) fn paint(&self, buf: &mut Buffer) {
        // `clone_from` on the cells reuses `buf`'s allocation.
        buf.content.clone_from(&self.frozen.content);
        if !self.moved {
            return;
        }
        for (row, from, to) in self.rows() {
            for col in from..=to {
                if let Some(cell) = buf.cell_mut((col, row)) {
                    cell.modifier.toggle(Modifier::REVERSED);
                }
            }
        }
    }

    /// The selected text: each row's cells from its first selected column to
    /// its last, trailing blanks dropped, rows joined by `\n`. A wide
    /// character half inside the selection is taken whole.
    fn text(&self) -> String {
        let mut out = String::new();
        let left = self.frozen.area.left();
        for (i, (row, from, to)) in self.rows().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let start = out.len();
            let mut col = left;
            while col <= to {
                let Some(cell) = self.frozen.cell((col, row)) else {
                    break;
                };
                let symbol = cell.symbol();
                let width = u16::try_from(symbol.width()).unwrap_or(1).max(1);
                if col.saturating_add(width) > from {
                    out.push_str(symbol);
                }
                col = col.saturating_add(width);
            }
            let kept = out
                .get(start..)
                .map_or(0, |line| line.trim_end_matches(' ').len());
            out.truncate(start + kept);
        }
        out
    }

    /// Each selected row with its first and last selected column, top to
    /// bottom: from the earlier end to the later one in reading order, every
    /// row in between whole.
    fn rows(&self) -> impl Iterator<Item = (u16, u16, u16)> + '_ {
        let ((c0, r0), (c1, r1)) = if (self.anchor.1, self.anchor.0) <= (self.head.1, self.head.0)
        {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        let area = self.frozen.area;
        let (left, right) = (area.left(), area.right().saturating_sub(1));
        (r0..=r1).map(move |row| {
            let from = if row == r0 { c0 } else { left };
            let to = if row == r1 { c1 } else { right };
            (row, from, to)
        })
    }
}

/// `at` moved inside `area`: a report from past the edge (a drag out of the
/// window) selects up to it.
fn clamp(area: Rect, (col, row): At) -> At {
    (
        col.clamp(area.left(), area.right().saturating_sub(1).max(area.left())),
        row.clamp(area.top(), area.bottom().saturating_sub(1).max(area.top())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    /// A frozen frame holding `lines`, as wide as the widest.
    fn frame(width: u16, lines: &[&str]) -> Buffer {
        let height = u16::try_from(lines.len()).expect("few lines");
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        for (y, line) in (0u16..).zip(lines) {
            buf.set_string(0, y, line, Style::default());
        }
        buf
    }

    fn drag(buf: Buffer, from: At, to: At) -> Release {
        let mut d = Drag::new(buf, from);
        d.extend(to);
        d.release(to)
    }

    #[test]
    fn a_press_released_in_place_is_a_click() {
        let d = Drag::new(frame(10, &["hello"]), (2, 0));
        assert_eq!(d.release((2, 0)), Release::Click { col: 2, row: 0 });
    }

    #[test]
    fn a_drag_back_to_its_start_copies_that_cell() {
        let mut d = Drag::new(frame(10, &["hello"]), (1, 0));
        assert!(d.extend((3, 0)));
        assert_eq!(d.release((1, 0)), Release::Copy("e".into()));
    }

    #[test]
    fn a_drag_on_one_row_copies_the_cells_between_both_ends() {
        let buf = frame(20, &["the quick fox"]);
        assert_eq!(drag(buf, (4, 0), (8, 0)), Release::Copy("quick".into()));
    }

    #[test]
    fn a_drag_backwards_copies_the_same_cells() {
        let buf = frame(20, &["the quick fox"]);
        assert_eq!(drag(buf, (8, 0), (4, 0)), Release::Copy("quick".into()));
    }

    #[test]
    fn rows_in_between_are_whole_and_trailing_blanks_dropped() {
        let buf = frame(12, &["first line", "middle", "last line"]);
        assert_eq!(
            drag(buf, (6, 0), (3, 2)),
            Release::Copy("line\nmiddle\nlast".into())
        );
    }

    #[test]
    fn an_upward_drag_reads_top_to_bottom() {
        let buf = frame(12, &["first line", "middle", "last line"]);
        assert_eq!(
            drag(buf, (3, 2), (6, 0)),
            Release::Copy("line\nmiddle\nlast".into())
        );
    }

    #[test]
    fn a_blank_row_in_between_stays_an_empty_line() {
        let buf = frame(8, &["ab", "", "cd"]);
        assert_eq!(drag(buf, (0, 0), (1, 2)), Release::Copy("ab\n\ncd".into()));
    }

    #[test]
    fn a_drag_over_blanks_copies_nothing() {
        let buf = frame(10, &["ab"]);
        assert_eq!(drag(buf, (4, 0), (8, 0)), Release::Nothing);
    }

    #[test]
    fn a_wide_character_is_taken_once_and_whole() {
        // `漢` covers columns 1 and 2; the selection starts on its second
        // half and ends on the first half of `字`.
        let buf = frame(10, &["a漢b字c"]);
        assert_eq!(drag(buf, (2, 0), (4, 0)), Release::Copy("漢b字".into()));
    }

    #[test]
    fn a_report_past_the_edge_selects_to_it() {
        let buf = frame(5, &["abcde", "fghij"]);
        assert_eq!(
            drag(buf, (3, 0), (200, 200)),
            Release::Copy("de\nfghij".into())
        );
    }

    #[test]
    fn paint_reverses_the_selection_on_the_frozen_frame() {
        let frozen = frame(6, &["abcdef", "ghijkl"]);
        let mut d = Drag::new(frozen.clone(), (4, 0));
        d.extend((1, 1));
        // A frame the view drew since: paint puts the frozen one back.
        let mut buf = frame(6, &["xxxxxx", "xxxxxx"]);
        d.paint(&mut buf);
        let reversed = |col: u16, row: u16| {
            buf.cell((col, row))
                .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
        };
        let symbol = |col: u16, row: u16| buf.cell((col, row)).map(|c| c.symbol().to_owned());
        assert_eq!(symbol(0, 0).as_deref(), Some("a"));
        assert_eq!(symbol(5, 1).as_deref(), Some("l"));
        let selected: Vec<At> = (0..2)
            .flat_map(|row| (0..6).map(move |col| (col, row)))
            .filter(|&(c, r)| reversed(c, r))
            .collect();
        assert_eq!(selected, [(4, 0), (5, 0), (0, 1), (1, 1)]);
    }

    #[test]
    fn a_press_alone_paints_no_selection() {
        let d = Drag::new(frame(4, &["abcd"]), (1, 0));
        let mut buf = frame(4, &["xxxx"]);
        d.paint(&mut buf);
        assert!(
            buf.content
                .iter()
                .all(|c| !c.modifier.contains(Modifier::REVERSED))
        );
        assert_eq!(buf.cell((1, 0)).map(|c| c.symbol().to_owned()).as_deref(), Some("b"));
    }

    #[test]
    fn a_resize_ends_the_fit() {
        let d = Drag::new(frame(4, &["abcd"]), (1, 0));
        assert!(d.fits(Rect::new(0, 0, 4, 1)));
        assert!(!d.fits(Rect::new(0, 0, 5, 1)));
    }
}
