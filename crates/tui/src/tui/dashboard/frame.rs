//! The dashboard's whole frame: the header, the list and the key-hint row.

use ratatui::style::{Color, Style};
use ratatui::text::Line;

use super::header::{Title, header_lines_hits};
use super::list::{ListInputs, ListState, list_lines};
use crate::tui::text::{clean_inline, cut_right};

/// The dashboard's key hints, as the spec draws them.
pub const DASHBOARD_HINTS: &str = "↑/↓ select · enter details · l log · tab/⇧tab list · / filter · a all targets · esc close dashboard · ctrl-c stop";

/// Rows the frame keeps besides the list: four of header, a blank and the
/// hint row.
const CHROME_ROWS: u16 = 6;
/// The header's rows, and the tab row's place among them.
const HEADER_ROWS: u16 = 4;
const TABS_ROW: u16 = 2;

/// The dashboard at `cols` × `rows`: the header's four rows, the list of
/// `st`'s tab, a blank and `hints` cut to fit. Exactly `rows` lines, none
/// wider than `cols`. The gauges and tab counts are `inp.model`'s, in
/// `st.scope`; `title` is the caller's, since its done figure and total
/// are the line's.
pub fn dashboard_frame(
    title: &Title<'_>,
    inp: &ListInputs<'_>,
    st: &mut ListState,
    hints: &str,
    cols: u16,
    rows: u16,
) -> Vec<Line<'static>> {
    let gauges = inp.model.gauges();
    let counts = inp.model.counts(st.scope);
    let (header, _, tabs) = header_lines_hits(title, &gauges, &counts, st.tab(), cols);
    let mut out: Vec<Line<'static>> = header.into_iter().collect();
    let list_rows = rows.saturating_sub(CHROME_ROWS);
    out.extend(list_lines(inp, st, cols, list_rows));
    // What a click can hit: the tab row (third, kept while the hint row
    // still fits under it) and the list's rows, under its column heads
    // when it has room for them.
    st.drawn.tabs_row = (rows >= 4).then_some(TABS_ROW);
    st.drawn.tabs = tabs;
    st.drawn.rows_top = HEADER_ROWS + u16::from(list_rows > 1);
    out.push(Line::default());
    let hint = cut_right(&clean_inline(hints), usize::from(cols).saturating_sub(2)).into_owned();
    out.push(Line::styled(
        format!("  {hint}"),
        Style::default().fg(Color::DarkGray),
    ));
    let rows = usize::from(rows);
    // Too short for the chrome: keep the top, and the hint row last.
    if out.len() > rows {
        let hint = out.pop();
        out.truncate(rows.saturating_sub(1));
        out.extend(hint.filter(|_| rows > 0));
    }
    out
}

#[cfg(test)]
mod tests {
    use hcore::events::{BuildEvent, BuildEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Text;
    use ratatui::widgets::Paragraph;

    use super::*;
    use crate::tui::dashboard::list::{LastLines, WaitWords, Waits};
    use crate::tui::dashboard::model::Model;
    use crate::tui::text::Redact;

    struct NoWaits;

    impl Waits for NoWaits {
        fn wait_words(&self, _: &str, _: u64) -> Option<WaitWords> {
            None
        }
    }

    fn model() -> Model {
        let mut m = Model::default();
        let mut t = 0;
        let mut ev = |kind: BuildEventKind| {
            t += 100;
            m.apply(
                &BuildEvent {
                    at_unix_ms: 1_000 + t,
                    kind,
                },
                &Redact::default(),
            );
        };
        ev(BuildEventKind::Matched {
            addrs: ["//a:one", "//a:two", "//b:three"]
                .map(str::to_owned)
                .to_vec(),
            complete: true,
        });
        for a in ["//a:one", "//a:two"] {
            ev(BuildEventKind::ResultStart { addr: a.into() });
        }
        ev(BuildEventKind::ResultEnd {
            cancelled: false,
            addr: "//a:one".into(),
            error: None,
            upstream_of: None,
            exit_status: None,
            log_tail: None,
            timed_out: None,
        });
        ev(BuildEventKind::PoolUsage {
            pool: hcore::capacity::SANDBOX.into(),
            held: 1,
            capacity: 4,
            waiting: 0,
            removals_pending: 0,
        });
        m
    }

    /// The frame as a terminal shows it: header, list, blank, hints, in
    /// exactly the terminal's rows.
    #[test]
    fn dashboard_frame_snapshot() {
        const COLS: u16 = 80;
        const ROWS: u16 = 14;
        let m = model();
        let lines = LastLines::default();
        let inp = ListInputs {
            model: &m,
            waits: &NoWaits,
            lines: &lines,
            now_ms: 2_000,
            redact: &Redact::default(),
        };
        let title = Title {
            command: "heph r build //...",
            session: Some("02kf5"),
            clock: "1s",
            done: 1,
            total: Some(3),
            provisional: false,
            standing: None,
        };
        let mut st = ListState::default();
        let frame = dashboard_frame(&title, &inp, &mut st, DASHBOARD_HINTS, COLS, ROWS);
        assert_eq!(frame.len(), usize::from(ROWS));
        let mut term = Terminal::new(TestBackend::new(COLS, ROWS)).expect("terminal");
        term.draw(|f| f.render_widget(Paragraph::new(Text::from(frame)), f.area()))
            .expect("draw");
        let buf = term.backend().buffer();
        let drawn: Vec<String> = (0..ROWS)
            .map(|y| {
                let row: String = (0..COLS)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect();
                row.trim_end().to_owned()
            })
            .collect();
        let want = [
            " heph r build //...  session 02kf5   1s · 1 / 3 done            sandbox █░░░ 1/4",
            "",
            "  All 3    ▶ Running 0    ⧗ Pending 2    ✗ Failed 0    ✓ Done 1    ● Cached 0",
            "──━━━━━─────────────────────────────────────────────────────────────────────────",
            "     TARGET                        PHASE              TIME LAST LINE",
            " ▸ ✓ //a:one                       done               0.0s",
            "   · //a:two                       pending",
            "   · //b:three                     pending",
            "",
            "",
            "",
            "                                    1–3 of 3",
            "",
            "  ↑/↓ select · enter details · l log · tab/⇧tab list · / filter · a all targets…",
        ];
        assert_eq!(drawn, want, "{}", drawn.join("\n"));

        // Shorter than its chrome: still exactly `rows`, the hints last.
        for rows in 0..=8u16 {
            let frame = dashboard_frame(&title, &inp, &mut st, DASHBOARD_HINTS, COLS, rows);
            assert_eq!(frame.len(), usize::from(rows), "{rows} rows");
            if let Some(last) = frame.last() {
                let t: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(t.starts_with("  ↑/↓ select"), "{rows} rows: {t:?}");
            }
        }
    }
}
