//! The line: one row that says how the build is going.
//!
//! Left to right: spinner and clock, the counts, the CPU the running targets
//! reserved, a standing approval decision, the spotlight, and the hint for
//! the dashboard right-aligned. It never wraps (I2). As the terminal
//! narrows, segments go in a fixed order — the spotlight (shortened first),
//! then the CPU figure, then
//! the words collapse to short forms; below [`BARE_BELOW`] columns only the
//! clock and the counts are left. The clock, the done count, the failed count
//! and a standing decision never drop above that.
//!
//! Every tier is tried from the one the width selects downwards, so a tier
//! whose content overflows (large counts, a long standing indicator) falls to
//! the next instead of overrunning.

use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::Spot;
use crate::tui::session::ApprovalStanding;
use crate::tui::text::{clean_inline, cut_left, cut_right, take_width, width};

/// Below this many columns the line is the clock and the counts, cut hard.
pub(super) const BARE_BELOW: usize = 20;
/// At this many columns and below the hint is the bare glyph.
const HINT_GLYPH_AT: usize = 40;
/// The fewest columns a shortened spotlight keeps of its name.
const SPOT_NAME_MIN: usize = 12;
/// The fewest columns a shortened spotlight keeps of its detail.
const SPOT_DETAIL_MIN: usize = 8;

const SEP: &str = " · ";
const HINT: &str = "tab ⇥";
const HINT_GLYPH: &str = "⇥";
/// [`HINT`] on an attached viewer, from 70 columns up.
const HINT_DETACH: &str = "q detach · tab ⇥";
/// After the counts of a finished run `--dashboard` holds.
const HELD: &str = "q to quit · tab dashboard";

/// How the run stands: still going (with this frame's spinner), or over.
#[derive(Debug, Clone, Copy)]
pub(super) enum Status<'a> {
    Running(&'a str),
    /// `stopped`: the user stopped it; the head says so, as the final
    /// summary does.
    Finished {
        failed: bool,
        stopped: bool,
    },
}

/// What a build's line counts.
#[derive(Debug)]
pub(super) struct BuildCounts {
    pub(super) done: usize,
    /// The total and whether it is provisional (`~`, the matcher still
    /// streams). `None` before any `Matched` event: `{done} done`.
    pub(super) total: Option<(usize, bool)>,
    pub(super) cached: usize,
    /// Admission, result-lock and scratch-lock waits together.
    pub(super) waiting: usize,
    pub(super) failed: usize,
    /// `cpu 11/12`: what the running targets reserved, of the pool.
    pub(super) cpu: Option<String>,
}

/// A total as the line and the box spell it: `412`, or `~412` while it is
/// provisional.
pub(super) fn total(n: usize, provisional: bool) -> impl std::fmt::Display {
    std::fmt::from_fn(move |f| match provisional {
        true => write!(f, "~{n}"),
        false => write!(f, "{n}"),
    })
}

/// What the line shows after the clock.
#[derive(Debug)]
pub(super) enum Body<'a> {
    Build(BuildCounts),
    /// A command whose header is a segment of its own (`heph gc`).
    Text(Cow<'a, str>),
}

/// One frame of the line, gathered by the view.
#[derive(Debug)]
pub(super) struct LineModel<'a> {
    pub(super) status: Status<'a>,
    /// Unpadded: `1m05s`.
    pub(super) clock: String,
    pub(super) body: Body<'a>,
    pub(super) standing: Option<ApprovalStanding>,
    pub(super) spot: Option<Spot>,
    /// Whether the command has a dashboard for `tab` to open.
    pub(super) hint: bool,
    /// An attached viewer (`heph session attach`): the hint says `q`
    /// detaches, where there is room for it.
    pub(super) detach: bool,
    /// The finished run is held (`--dashboard`): the line says how to leave.
    pub(super) held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// 100 columns and up: everything, the spotlight shortened to fit.
    Full,
    /// 70 to 99: no spotlight.
    NoSpot,
    /// 50 to 69: short words, no CPU.
    Short,
    /// 20 to 49: `128/412 3w 0f`, no cached count.
    Compact,
}

impl Tier {
    const ALL: [Tier; 4] = [Tier::Full, Tier::NoSpot, Tier::Short, Tier::Compact];

    fn for_width(cols: usize) -> Option<Self> {
        match cols {
            100.. => Some(Tier::Full),
            70.. => Some(Tier::NoSpot),
            50.. => Some(Tier::Short),
            BARE_BELOW.. => Some(Tier::Compact),
            _ => None,
        }
    }
}

/// One segment: its spans and their width.
struct Seg {
    spans: Vec<Span<'static>>,
    width: usize,
}

impl Seg {
    fn styled(text: String, style: Style) -> Self {
        Self {
            width: width(&text),
            spans: vec![Span::styled(text, style)],
        }
    }

    fn plain(text: String) -> Self {
        Self::styled(text, Style::default())
    }
}

fn dim() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM)
}

fn failed_style(failed: usize) -> Style {
    if failed > 0 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn standing_style() -> Style {
    Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD)
}

/// The line for `m` in `cols` columns. Never wider than `cols`.
pub(super) fn render(m: &LineModel<'_>, cols: u16) -> Line<'static> {
    let cols = usize::from(cols);
    if let Some(start) = Tier::for_width(cols) {
        for tier in Tier::ALL.into_iter().filter(|t| *t >= start) {
            if let Some(line) = layout(m, tier, cols) {
                return line;
            }
        }
    }
    bare(m, cols)
}

/// The status glyph and the clock: `⠹ 1m05s`, `✓ 1m42s`.
fn head(m: &LineModel<'_>) -> Seg {
    let (glyph, style) = match m.status {
        Status::Running(spinner) => (clean_inline(spinner).into_owned(), Style::default()),
        Status::Finished {
            failed: false,
            stopped: false,
        } => ("✓".to_owned(), Style::default().fg(Color::Green)),
        Status::Finished { .. } => (
            "✗".to_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let clock = Span::styled(
        m.clock.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    );
    let mut spans = vec![Span::styled(glyph.clone(), style), Span::raw(" ")];
    let mut w = width(&glyph) + 1 + width(&m.clock);
    if matches!(m.status, Status::Finished { stopped: true, .. }) {
        spans.push(Span::styled(STOPPED, style));
        spans.push(Span::raw(" · "));
        w += width(STOPPED) + 3;
    }
    spans.push(clock);
    Seg { width: w, spans }
}

/// What a stopped run's head says before its clock.
const STOPPED: &str = "stopped";

/// The required segments of `tier`, after the head: counts, CPU, standing.
fn counts(m: &LineModel<'_>, tier: Tier) -> Vec<Seg> {
    let running = matches!(m.status, Status::Running(_));
    let mut segs = Vec::with_capacity(6);
    match &m.body {
        // Cut to fit by the caller: a gc segment is one piece of text.
        Body::Text(_) => {}
        Body::Build(c) => {
            let done = match (c.total, tier) {
                (Some((n, p)), Tier::Full | Tier::NoSpot) => {
                    format!("{} / {} done", c.done, total(n, p))
                }
                (Some((n, p)), _) => format!("{}/{}", c.done, total(n, p)),
                (None, Tier::Full | Tier::NoSpot) => format!("{} done", c.done),
                (None, _) => c.done.to_string(),
            };
            segs.push(Seg::plain(done));
            if tier != Tier::Compact {
                segs.push(Seg::plain(format!("{} cached", c.cached)));
            }
            if running {
                segs.push(Seg::plain(match tier {
                    Tier::Full | Tier::NoSpot => format!("{} waiting", c.waiting),
                    Tier::Short => format!("{} wait", c.waiting),
                    Tier::Compact => format!("{}w", c.waiting),
                }));
            }
            let failed = match tier {
                Tier::Full | Tier::NoSpot => format!("{} failed", c.failed),
                Tier::Short => format!("{} fail", c.failed),
                Tier::Compact => format!("{}f", c.failed),
            };
            segs.push(Seg::styled(failed, failed_style(c.failed)));
            if running
                && matches!(tier, Tier::Full | Tier::NoSpot)
                && let Some(cpu) = &c.cpu
            {
                segs.push(Seg::plain(cpu.clone()));
            }
        }
    }
    if running && let Some(s) = m.standing {
        let text = match (tier, s) {
            (Tier::Compact, ApprovalStanding::Approve) => "auto-Y",
            (Tier::Compact, ApprovalStanding::Reject) => "auto-N",
            _ => s.indicator(),
        };
        segs.push(Seg::styled(text.to_owned(), standing_style()));
    }
    segs
}

/// The spotlight in at most `room` columns: whole, else its name cut from
/// the left (the end of an address names it) and then its detail from the
/// right, each down to a floor; `None` when even that does not fit.
fn spot(spot: &Spot, room: usize) -> Option<Seg> {
    let icon_w = width(spot.icon) + 1;
    let name_w = width(&spot.name);
    let detail_w = match spot.detail.is_empty() {
        true => 0,
        false => width(&spot.detail) + 1,
    };
    let whole = icon_w + name_w + detail_w;
    let fit = |name: Cow<'_, str>, detail: Cow<'_, str>| {
        let text = match detail.is_empty() {
            true => format!("{} {name}", spot.icon),
            false => format!("{} {name} {detail}", spot.icon),
        };
        Seg::styled(text, spot.style)
    };
    if whole <= room {
        return Some(fit(Cow::Borrowed(&spot.name), Cow::Borrowed(&spot.detail)));
    }
    let name_min = name_w.min(SPOT_NAME_MIN);
    let name_room = room.checked_sub(icon_w + detail_w)?;
    if name_room >= name_min {
        return Some(fit(
            cut_left(&spot.name, name_room),
            Cow::Borrowed(&spot.detail),
        ));
    }
    if detail_w == 0 {
        return None;
    }
    let detail_room = room.checked_sub(icon_w + name_min + 1)?;
    if detail_room < SPOT_DETAIL_MIN.min(detail_w - 1) {
        return None;
    }
    Some(fit(
        cut_left(&spot.name, name_min),
        cut_right(&spot.detail, detail_room),
    ))
}

fn joined_width(segs: &[Seg], sep: &str) -> usize {
    let seps = segs.len().saturating_sub(1) * width(sep);
    segs.iter().map(|s| s.width).sum::<usize>() + seps
}

/// `m` laid out at `tier` in `cols` columns, or `None` when what the tier
/// must show does not fit.
fn layout(m: &LineModel<'_>, tier: Tier, cols: usize) -> Option<Line<'static>> {
    let running = matches!(m.status, Status::Running(_));
    let hint = match (running && m.hint, tier) {
        (false, _) => None,
        (true, Tier::Compact) if cols <= HINT_GLYPH_AT => Some(HINT_GLYPH),
        (true, Tier::Full | Tier::NoSpot) if m.detach => Some(HINT_DETACH),
        (true, _) => Some(HINT),
    };
    // The hint keeps at least one blank column between it and the rest.
    let room = cols.checked_sub(hint.map_or(0, |h| width(h) + 1))?;
    let sep = match tier {
        Tier::Compact => " ",
        _ => SEP,
    };
    let sep_w = width(sep);

    let mut segs = vec![head(m)];
    segs.extend(counts(m, tier));
    if let Body::Text(text) = &m.body {
        // The command's own segment fills what is left, cut from the right.
        let left = room.checked_sub(joined_width(&segs, sep) + sep_w)?;
        let text = clean_inline(text);
        segs.insert(1, Seg::plain(cut_right(&text, left).into_owned()));
    }
    let base = joined_width(&segs, sep);
    if base > room {
        return None;
    }

    // The whole spotlight when it fits; else it is shortened, then it goes.
    let spot_seg = m
        .spot
        .as_ref()
        .filter(|_| running && tier == Tier::Full)
        .and_then(|s| spot(s, room.checked_sub(base + sep_w)?));
    segs.extend(spot_seg);
    if m.held && !running && joined_width(&segs, sep) + sep_w + width(HELD) <= room {
        segs.push(Seg::styled(HELD.to_owned(), dim()));
    }

    let mut spans = Vec::with_capacity(segs.len() * 3 + 2);
    let mut used = 0;
    for (i, seg) in segs.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(sep));
            used += sep_w;
        }
        used += seg.width;
        spans.extend(seg.spans);
    }
    if let Some(h) = hint {
        let pad = cols.saturating_sub(used + width(h));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(h, dim()));
    }
    Some(Line::from(spans))
}

/// Under [`BARE_BELOW`] columns, or when no tier fits: the clock and the
/// counts, cut hard.
fn bare(m: &LineModel<'_>, cols: usize) -> Line<'static> {
    let running = matches!(m.status, Status::Running(_));
    let text = match &m.body {
        Body::Build(c) => {
            let done = match c.total {
                Some((n, p)) => format!("{}/{}", c.done, total(n, p)),
                None => c.done.to_string(),
            };
            match running {
                true => format!("{} {done} {}w {}f", m.clock, c.waiting, c.failed),
                false => format!("{} {done} {}f", m.clock, c.failed),
            }
        }
        Body::Text(t) => format!("{} {}", m.clock, clean_inline(t)),
    };
    Line::from(take_width(&text, cols).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn model(spot: Option<Spot>) -> LineModel<'static> {
        LineModel {
            status: Status::Running("⠹"),
            clock: "1m05s".to_owned(),
            body: Body::Build(BuildCounts {
                done: 128,
                total: Some((412, false)),
                cached: 96,
                waiting: 3,
                failed: 0,
                cpu: Some("cpu 11/12".to_owned()),
            }),
            standing: None,
            spot,
            hint: true,
            detach: false,
            held: false,
        }
    }

    /// An attached viewer's line says `q` detaches where the full hint fits,
    /// and narrows to the live line's hint below that: the rest is the same.
    #[test]
    fn attached_line_hint_says_detach() {
        let live = model(Some(run_spot("//a:b")));
        let attached = LineModel {
            detach: true,
            ..model(Some(run_spot("//a:b")))
        };
        let at = |m: &LineModel<'_>, cols: u16| text(&render(m, cols));
        let wide = at(&attached, 120);
        assert!(wide.ends_with(HINT_DETACH), "{wide}");
        assert_eq!(
            wide.trim_end_matches(HINT_DETACH).trim_end(),
            at(&live, 120).trim_end_matches(HINT).trim_end()
        );
        for cols in [60, 40, 25] {
            assert_eq!(at(&attached, cols), at(&live, cols), "{cols}");
        }
    }

    fn run_spot(name: &str) -> Spot {
        Spot {
            icon: "▶",
            style: Style::default(),
            name: name.to_owned(),
            detail: "42s".to_owned(),
        }
    }

    /// The mockups of the spec, at their widths.
    #[test]
    fn line_matches_the_mockups() {
        let m = model(Some(run_spot("//go/cmd/heph-govet:build")));
        let at = |cols: u16| text(&render(&m, cols));
        assert_eq!(
            at(120).trim_end_matches(HINT).trim_end(),
            "⠹ 1m05s · 128 / 412 done · 96 cached · 3 waiting · 0 failed · cpu 11/12 · ▶ //go/cmd/heph-govet:build 42s"
        );
        assert!(at(100).contains("▶ …"), "{}", at(100));
        assert_eq!(
            at(80).trim_end_matches(HINT).trim_end(),
            "⠹ 1m05s · 128 / 412 done · 96 cached · 3 waiting · 0 failed · cpu 11/12"
        );
        assert_eq!(
            at(60).trim_end_matches(HINT).trim_end(),
            "⠹ 1m05s · 128/412 · 96 cached · 3 wait · 0 fail"
        );
        assert_eq!(
            at(40).trim_end_matches(HINT_GLYPH).trim_end(),
            "⠹ 1m05s 128/412 3w 0f"
        );
        assert!(
            at(40).ends_with(" ⇥") && !at(40).contains("tab"),
            "{}",
            at(40)
        );
        assert_eq!(at(19), "1m05s 128/412 3w 0f");
        for cols in [120, 100, 80, 60, 40, 19] {
            assert_eq!(
                width(&at(cols)),
                usize::from(cols),
                "{cols}: the hint sits right"
            );
        }
    }

    /// A provisional total reads `~` at every tier, the bare one included;
    /// with no total yet, only the done count shows.
    #[test]
    fn done_count_spells_the_total() {
        let with_total = |total| {
            let mut m = model(None);
            if let Body::Build(c) = &mut m.body {
                c.total = total;
            }
            m
        };
        let provisional = with_total(Some((412, true)));
        let at = |m: &LineModel<'_>, cols: u16| text(&render(m, cols));
        assert!(
            at(&provisional, 120).contains(" 128 / ~412 done "),
            "{}",
            at(&provisional, 120)
        );
        assert!(
            at(&provisional, 60).contains(" 128/~412 "),
            "{}",
            at(&provisional, 60)
        );
        // Bare is cut hard: the `~` costs the last column.
        assert_eq!(at(&provisional, 19), "1m05s 128/~412 3w 0");
        let none = with_total(None);
        assert!(at(&none, 120).contains(" 128 done "), "{}", at(&none, 120));
        assert!(at(&none, 60).contains(" 128 · "), "{}", at(&none, 60));
        assert_eq!(at(&none, 19), "1m05s 128 3w 0f");
    }

    #[test]
    fn finished_line_is_the_summary() {
        let mut m = model(Some(run_spot("//a:b")));
        m.status = Status::Finished {
            failed: true,
            stopped: true,
        };
        m.standing = Some(ApprovalStanding::Approve);
        if let Body::Build(c) = &mut m.body {
            c.failed = 3;
        }
        // A stopped run's line says so, as its summary did.
        assert_eq!(
            text(&render(&m, 120)),
            "✗ stopped · 1m05s · 128 / 412 done · 96 cached · 3 failed"
        );
        m.status = Status::Finished {
            failed: true,
            stopped: false,
        };
        assert_eq!(
            text(&render(&m, 120)),
            "✗ 1m05s · 128 / 412 done · 96 cached · 3 failed"
        );
        // Held by `--dashboard`: how to leave, when it fits.
        m.held = true;
        assert_eq!(
            text(&render(&m, 120)),
            "✗ 1m05s · 128 / 412 done · 96 cached · 3 failed · q to quit · tab dashboard"
        );
        let narrow = text(&render(&m, 60));
        assert!(
            !narrow.contains("q to quit") && narrow.contains("3 fail"),
            "dropped before a count: {narrow}"
        );
    }

    /// The standing decision stays at every width its compact form fits in
    /// (`⠹ 1m05s 128/412 3w 0f auto-N ⇥`); narrower, the line is bare.
    #[test]
    fn standing_never_drops_above_bare() {
        let mut m = model(Some(run_spot("//a:b")));
        m.standing = Some(ApprovalStanding::Reject);
        for cols in 30..=300 {
            let t = text(&render(&m, u16::try_from(cols).expect("cols")));
            assert!(
                t.contains("auto-rejecting (N)") || t.contains("auto-N"),
                "{cols}: {t}"
            );
        }
    }

    #[test]
    fn text_body_is_cut_to_fit() {
        let m = LineModel {
            status: Status::Running("⠋"),
            clock: "3s".to_owned(),
            body: Body::Text(Cow::Borrowed(
                "12 targets · 40 revisions · 1.2 GiB freed · cache.db 4 MiB",
            )),
            standing: None,
            spot: None,
            hint: false,
            detach: false,
            held: false,
        };
        for cols in 1..=120_u16 {
            let t = text(&render(&m, cols));
            assert!(width(&t) <= usize::from(cols), "{cols}: {t}");
            assert!(!t.contains(HINT_GLYPH), "no dashboard, no hint: {t}");
        }
        assert_eq!(
            text(&render(&m, 120)),
            "⠋ 3s · 12 targets · 40 revisions · 1.2 GiB freed · cache.db 4 MiB"
        );
    }

    /// The widest column `present` holds at and below which it is gone for
    /// good, walking from 300 columns down to [`BARE_BELOW`]: a segment that
    /// left never comes back as the line narrows.
    fn drops_at(m: &LineModel<'_>, present: impl Fn(&str) -> bool) -> usize {
        let mut gone_from = None;
        for cols in (BARE_BELOW..=300).rev() {
            let t = text(&render(m, u16::try_from(cols).expect("cols")));
            match (present(&t), gone_from) {
                (false, None) => gone_from = Some(cols),
                (true, Some(g)) => panic!("came back at {cols} after leaving at {g}: {t}"),
                _ => {}
            }
        }
        gone_from.unwrap_or(0)
    }

    #[test]
    fn line_never_exceeds_width() {
        let spots = [
            run_spot("//go/cmd/heph-govet:build"),
            // Wide characters: two columns each.
            run_spot("//漢字/パッケージ:ターゲット"),
            // Combining marks: no column of their own.
            run_spot("//e\u{301}te\u{301}/cafe\u{301}:re\u{301}sume\u{301}"),
            Spot {
                icon: "⧗",
                style: Style::default(),
                name: "//中文:目标".to_owned(),
                detail: "waits for cpu 2 (1 free, 41s) — cleanup is behind: 2 removals pending"
                    .to_owned(),
            },
        ];
        for spot in spots {
            let mut m = model(Some(spot.clone()));
            m.standing = Some(ApprovalStanding::Approve);
            for cols in 0..=300_u16 {
                let line = render(&m, cols);
                let t = text(&line);
                assert!(width(&t) <= usize::from(cols), "{cols}: {t}");
                assert_eq!(line.width(), width(&t), "ratatui measures it alike");
                if usize::from(cols) < BARE_BELOW {
                    assert!(
                        "1m05s 128/412 3w 0f".starts_with(&t),
                        "{cols}: the clock and the counts only: {t}"
                    );
                    continue;
                }
                // The clock, the done count and the failed count never drop.
                assert!(t.contains("1m05s") && t.contains("128"), "{cols}: {t}");
                assert!(
                    t.contains("0 failed") || t.contains("0 fail") || t.contains("0f"),
                    "{cols}: {t}"
                );
            }

            // Segments drop in the stated order: the spotlight (shortened
            // first), the CPU figure, then the long words, then the cached
            // count.
            let shortened = drops_at(&m, |t| t.contains(&spot.text()));
            let spotlit = drops_at(&m, |t| t.contains(spot.icon));
            let cpu = drops_at(&m, |t| t.contains("cpu 11/12"));
            let words = drops_at(&m, |t| t.contains("3 waiting"));
            let cached = drops_at(&m, |t| t.contains("96 cached"));
            assert!(
                shortened >= spotlit && spotlit > cpu && cpu >= words && words > cached,
                "{shortened} {spotlit} {cpu} {words} {cached}"
            );
            // Gone below its tier at the latest; earlier when the content
            // overflows the tier and falls to the next.
            assert!(
                spotlit >= 99 && cpu >= 69 && cached >= 49,
                "{spotlit} {cpu} {cached}"
            );
        }
    }

    /// Text the line draws from the stream — an address, a failure, the
    /// approval prompt — reaches the terminal with no control
    /// byte left in it, whatever the stream carried.
    #[test]
    fn control_bytes_never_reach_terminal_line() {
        use crate::tui::app::TUIAppView;
        use crate::tui::approval::ApprovalCenter;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, end, start};

        let hostile = [
            "\x1b[?1049l",
            "\x1b[?1003h",
            "\x1b[2J",
            "\x1b]8;;http://x\x07link\x1b]8;;\x07",
            "\x1b]52;c;aGk=\x07",
            "\u{9b}31m",
            "\0",
            "\r",
            "\t",
            "\u{7f}",
        ];
        for bad in hostile {
            let addr = format!("//c:a{bad}b");
            let center = ApprovalCenter::new();
            let mut v =
                TuiProgressView::new(format!("Running {bad}//c:...")).with_approval(center.clone());
            v.apply(&at(0, start(&addr)));
            v.apply(&at(
                10,
                end(&addr, Some(&format!("boom{bad} 10%\r20%")), None),
            ));
            let build = center.build();
            let _ticket = center.request(&build, format!("//c:gate{bad}"), Vec::new());
            for cols in [30_u16, 80, 200] {
                for line in v.render("⠋", T0 + 1_000, cols, 24) {
                    for span in &line.spans {
                        assert!(
                            !span.content.chars().any(char::is_control),
                            "{bad:?} at {cols}: {:?}",
                            span.content
                        );
                    }
                }
            }
        }
    }

    /// The line never names a path into heph's home or a sandbox: the
    /// prompt row leaves the notice links to the box, and a failure is
    /// named by its target and exit status.
    #[test]
    fn no_home_path_on_the_line() {
        use crate::tui::app::TUIAppView;
        use crate::tui::approval::{ApprovalCenter, ApprovalNoticeView};
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, start};
        use hcore::events::BuildEventKind;

        const HOME: &str = "/home/u/.heph";
        const SANDBOX: &str = "/home/u/.heph/sandbox/ab12";
        let center = ApprovalCenter::new();
        let mut v = TuiProgressView::new("Running //...").with_approval(center.clone());
        v.apply(&at(0, start("//p:fail")));
        v.apply(&at(
            10,
            BuildEventKind::ResultEnd {
                duplicate: false,
                upstream_file: None,
                path: Vec::new(),
                left_behind: None,
                cancelled: false,
                addr: "//p:fail".into(),
                error: Some(format!("exec failed\n{SANDBOX}/run.sh: exit status: 101")),
                upstream_of: None,
                exit_status: Some("exit status: 101".into()),
                exit_code: None,
                log_tail: None,
                timed_out: None,
            },
        ));
        let build = center.build();
        let first = center.request(
            &build,
            "//p:gate".into(),
            vec![ApprovalNoticeView {
                name: "plan".into(),
                sources: vec![hcore::approval_notice::NoticeSource {
                    addr: "//p:plan".into(),
                    origin: hcore::approval_notice::NoticeOrigin::File {
                        path: format!("{HOME}/plan.txt"),
                        out_path: "plan.txt".into(),
                        x: false,
                        hashout: "0f0f".into(),
                    },
                }],
            }],
        );
        let second = center.request(&build, "//p:next".into(), Vec::new());
        let mut seen = String::new();
        for cols in [40_u16, 80, 120, 300] {
            for line in v.render("⠋", T0 + 1_000, cols, 24) {
                for span in &line.spans {
                    seen.push_str(&span.content);
                }
                seen.push('\n');
            }
        }
        assert!(seen.contains("//p:gate"), "{seen}");
        assert!(
            !seen.contains(HOME) && !seen.contains("/sandbox/"),
            "{seen}"
        );
        // With the prompt answered the failure is spotlit, by its status.
        center.respond(first.seq, true, Vec::new());
        center.respond(second.seq, true, Vec::new());
        let line = v.render("⠋", T0 + 1_000, 300, 24);
        let t: String = line
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(t.contains("✗ //p:fail exit 101"), "{t}");
        assert!(!t.contains(HOME), "{t}");
    }

    /// I9's error-text clause: a root failure with no exit status is named
    /// by its error's first line, with heph's home redacted; the finished
    /// line splits roots from what they took down.
    #[test]
    fn no_home_path_in_a_failure_with_no_exit() {
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, start};
        use hcore::events::BuildEventKind;

        const HOME: &str = "/home/u/.heph";
        let fail =
            |addr: &str, error: String, upstream_of: Option<&str>| BuildEventKind::ResultEnd {
                duplicate: false,
                upstream_file: None,
                path: Vec::new(),
                left_behind: None,
                cancelled: false,
                addr: addr.into(),
                error: Some(error),
                upstream_of: upstream_of.map(str::to_owned),
                exit_status: None,
                exit_code: None,
                log_tail: None,
                timed_out: None,
            };
        let mut v = TuiProgressView::new("Running //...")
            .with_redact(crate::tui::text::Redact::of([HOME.into()]));
        for a in ["//p:root", "//p:down1", "//p:down2"] {
            v.apply(&at(0, start(a)));
        }
        v.apply(&at(
            10,
            fail(
                "//p:root",
                format!("staging {HOME}/sandbox/ab/in: denied\nmore"),
                None,
            ),
        ));
        v.apply(&at(
            11,
            fail("//p:down1", "dependency failed".into(), Some("//p:root")),
        ));
        v.apply(&at(
            12,
            fail("//p:down2", "dependency failed".into(), Some("//p:root")),
        ));
        let t: String = v
            .render("⠋", T0 + 1_000, 300, 24)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            t.contains("✗ //p:root staging <heph home>/sandbox/ab/in: denied"),
            "{t}"
        );
        assert!(!t.contains(HOME), "{t}");
        v.set_finished(T0 + 2_000);
        let last = v.final_line().expect("a summary");
        assert!(
            last.contains("3 failed (1 root, 2 because of it)"),
            "{last}"
        );
    }

    fn failed_end(
        addr: &str,
        upstream_of: Option<&str>,
        cancelled: bool,
    ) -> hcore::events::BuildEventKind {
        hcore::events::BuildEventKind::ResultEnd {
            duplicate: false,
            upstream_file: None,
            path: Vec::new(),
            left_behind: None,
            addr: addr.into(),
            error: Some(if cancelled { "cancelled" } else { "boom" }.into()),
            upstream_of: upstream_of.map(str::to_owned),
            exit_status: (!cancelled && upstream_of.is_none()).then(|| "exit status: 1".into()),
            exit_code: None,
            log_tail: None,
            timed_out: None,
            cancelled,
        }
    }

    fn all_text(lines: &[Line<'_>]) -> String {
        lines.iter().map(text).collect()
    }

    /// The number the line prints before the word `noun`.
    fn figure(line: &str, noun: &str) -> usize {
        let words: Vec<&str> = line.split_whitespace().collect();
        let at = words
            .iter()
            .position(|w| *w == noun)
            .unwrap_or_else(|| panic!("no {noun:?} in {line}"));
        let n = words.get(at.wrapping_sub(1)).copied().unwrap_or_default();
        n.parse()
            .unwrap_or_else(|_| panic!("{n:?} before {noun:?}: {line}"))
    }

    /// The line's `N / M done`.
    fn done_of(line: &str) -> (usize, usize) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let at = words.iter().position(|w| *w == "done").expect("done");
        let n = |k: usize| -> usize {
            words
                .get(at.wrapping_sub(k))
                .and_then(|w| w.parse().ok())
                .unwrap_or_else(|| panic!("no N / M done in {line}"))
        };
        assert_eq!(words.get(at.wrapping_sub(2)), Some(&"/"), "{line}");
        (n(3), n(1))
    }

    /// The line and the dashboard's tabs read one `Counts`: cached and
    /// failed equal those tabs, waiting the `⧗` rows of Pending, done the
    /// Done and Cached tabs together, in either scope.
    #[test]
    fn line_counts_match_tab_counts() {
        use crate::tui::app::TUIAppView;
        use crate::tui::dashboard::{RowState, Tab};
        use crate::tui::progress::test_events::{T0, admission, at, end, exec, lock, start};
        use crate::tui::progress::{CountScope, TuiProgressView};
        use hcore::events::{BuildEventKind, CacheOutcomeKind};

        let hit = |addr: &str| BuildEventKind::CacheOutcome {
            addr: addr.into(),
            outcome: CacheOutcomeKind::LocalHit,
            reason: None,
            stale_host_path: None,
        };
        let matched = [
            "//p:ok",
            "//p:hit",
            "//p:hit2",
            "//p:root",
            "//p:down",
            "//p:adm",
            "//p:lock",
            "//p:queued",
            "//p:run",
        ];
        let mut v = TuiProgressView::new("Running //p:...");
        let mut events = vec![BuildEventKind::Matched {
            addrs: matched.iter().map(|a| (*a).to_owned()).collect(),
            complete: true,
        }];
        for a in matched.iter().filter(|a| **a != "//p:queued") {
            events.push(start(a));
        }
        events.extend([
            exec("//p:ok"),
            end("//p:ok", None, None),
            hit("//p:hit"),
            end("//p:hit", None, None),
            hit("//p:hit2"),
            end("//p:hit2", None, None),
            // An unmatched dependency from the cache: All counts it.
            start("//d:hit"),
            hit("//d:hit"),
            end("//d:hit", None, None),
            failed_end("//p:root", None, false),
            failed_end("//p:down", Some("//p:root"), false),
            admission("//p:adm", "cpu", None, None),
            lock("//p:lock", Some(42)),
            exec("//p:run"),
        ]);
        for (i, e) in events.into_iter().enumerate() {
            v.apply(&at(i as u64, e));
        }

        // The line counts the matched scope whichever scope the dashboard's
        // list was left on.
        let scope = CountScope::Matched;
        let want = (3, 9, 2, 2, 2);
        for list_scope in [CountScope::Matched, CountScope::All] {
            v.list.scope = list_scope;
            let line = all_text(&v.render("⠋", T0 + 1_000, 200, 40));
            let m = v.state.dashboard();
            let tab = |t: Tab| m.tab_len(t, scope);
            let blocked = m
                .rows(Tab::Pending, scope)
                .filter(|&id| m.record(id).is_some_and(|r| r.state == RowState::Blocked))
                .count();
            let (done, total) = done_of(&line);
            let got = (
                done,
                total,
                figure(&line, "cached"),
                figure(&line, "waiting"),
                figure(&line, "failed"),
            );
            assert_eq!(got, want, "{scope:?}: {line}");
            assert_eq!(done, tab(Tab::Done) + tab(Tab::Cached), "{scope:?}: {line}");
            assert_eq!(total, tab(Tab::All), "{scope:?}: {line}");
            assert_eq!(got.2, tab(Tab::Cached), "{scope:?}: {line}");
            assert_eq!(got.3, blocked, "{scope:?}: {line}");
            assert_eq!(got.4, tab(Tab::Failed), "{scope:?}: {line}");
        }
    }

    /// Root and upstream failures during the build stay on the one row: the
    /// failed count and the spotlight update, nothing else is drawn, and the
    /// blocks wait for the report.
    #[test]
    fn failure_prints_nothing_until_report() {
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, start};

        let mut v = TuiProgressView::new("Running //...");
        for a in ["//p:root", "//p:down1", "//p:down2", "//p:other"] {
            v.apply(&at(0, start(a)));
        }
        v.apply(&at(10, failed_end("//p:root", None, false)));
        v.apply(&at(11, failed_end("//p:down1", Some("//p:root"), false)));
        v.apply(&at(12, failed_end("//p:down2", Some("//p:root"), false)));
        assert_eq!(v.rows(40), 1);
        let lines = v.render("⠋", T0 + 1_000, 200, 40);
        assert_eq!(lines.len(), 1, "one row while failures arrive");
        let t = all_text(&lines);
        assert!(t.contains("3 failed"), "{t}");
        assert!(t.contains("✗ //p:root exit 1"), "{t}");
        assert!(!t.contains("dependency"), "no block mid-build: {t}");
    }

    /// A stop is not a failure: after Ctrl-C the cancelled ends count in
    /// neither the failed nor the roots, never take the spotlight, and the
    /// finished line still reads ✗.
    #[test]
    fn a_stop_is_not_a_root_failure() {
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, start};

        let mut v = TuiProgressView::new("Running //...");
        for a in ["//p:root", "//p:down", "//p:stopped", "//p:behind"] {
            v.apply(&at(0, start(a)));
        }
        v.apply(&at(10, failed_end("//p:root", None, false)));
        v.apply(&at(11, failed_end("//p:down", Some("//p:root"), false)));
        v.apply(&at(20, failed_end("//p:stopped", None, true)));
        v.apply(&at(21, failed_end("//p:behind", Some("//p:stopped"), true)));
        let t = all_text(&v.render("⠋", T0 + 1_000, 200, 40));
        assert!(t.contains("2 failed"), "{t}");
        assert!(t.contains("✗ //p:root"), "the stop never takes it: {t}");
        v.set_finished(T0 + 2_000);
        let last = v.final_line().expect("a summary");
        assert!(
            last.contains("2 failed (1 root, 1 because of it)"),
            "{last}"
        );

        // Only stops: no failure to split, but not a success either.
        let mut v = TuiProgressView::new("Running //...");
        v.apply(&at(0, start("//p:stopped")));
        v.apply(&at(20, failed_end("//p:stopped", None, true)));
        v.set_finished(T0 + 2_000);
        let last = v.final_line().expect("a summary");
        assert!(last.starts_with("✗ "), "{last}");
        assert!(last.contains("0 failed"), "{last}");
        assert!(!last.contains("root"), "{last}");
    }

    /// A user stop (a ctrl-c that stopped, or a signal) says so first, keeps
    /// the clock, and still reports the roots that failed before it. The
    /// same cancelled ends with no user stop (fail-fast) do not say it.
    #[test]
    fn a_user_stop_says_stopped() {
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, start};

        let run = || {
            let mut v = TuiProgressView::new("Running //...");
            for a in ["//p:a", "//p:b", "//p:c"] {
                v.apply(&at(0, start(a)));
            }
            v.apply(&at(10, failed_end("//p:a", None, false)));
            v.apply(&at(11, failed_end("//p:b", None, false)));
            v.apply(&at(20, failed_end("//p:c", None, true)));
            v
        };

        let mut v = run();
        v.set_stopped();
        v.set_finished(T0 + 2_000);
        let last = v.final_line().expect("a summary");
        assert!(last.starts_with("✗ stopped · 2.000s · "), "{last}");
        assert!(last.contains("2 failed (2 roots)"), "{last}");

        // Fail-fast cancelled `//p:c` too, but nobody stopped the run.
        let mut v = run();
        v.set_finished(T0 + 2_000);
        let last = v.final_line().expect("a summary");
        assert!(last.starts_with("✗ 2.000s · "), "{last}");
        assert!(!last.contains("stopped"), "{last}");

        // A stop with nothing failed is still not a success.
        let mut v = TuiProgressView::new("Running //...");
        v.apply(&at(0, start("//p:a")));
        v.apply(&at(5, failed_end("//p:a", None, true)));
        v.set_stopped();
        v.set_finished(T0 + 2_000);
        let last = v.final_line().expect("a summary");
        assert!(last.starts_with("✗ stopped · 2.000s · "), "{last}");

        // A stop before anything matched: the line still says so, with no
        // counts to follow; with no stop, such a run prints nothing.
        let mut v = TuiProgressView::new("Running //...");
        v.set_stopped();
        v.set_finished(T0 + 2_000);
        let last = v.final_line().expect("a line for the stop");
        assert!(
            last.starts_with("✗ stopped · ") && !last.ends_with(" · "),
            "{last}"
        );
        let mut v = TuiProgressView::new("Running //...");
        v.set_finished(T0 + 2_000);
        assert_eq!(v.final_line(), None);
    }

    /// Finished on the dashboard, its title row says how, worded as the
    /// line's last line: `✓ done` green bold, `✗ 2 failed` or `✗ stopped`
    /// red bold, ahead of the frozen clock. Nothing while the run goes on.
    #[test]
    fn the_dashboard_title_says_the_run_finished() {
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, end, start};
        use ratatui::style::{Color, Modifier};

        let title = |v: &mut TuiProgressView| {
            v.render_full(T0 + 3_000, 120, 30)
                .into_iter()
                .next()
                .expect("a title row")
        };
        let verdict = |row: &Line<'_>| {
            row.spans
                .iter()
                .find(|s| s.content.contains('✓') || s.content.contains('✗'))
                .map(|s| {
                    (
                        s.content.trim().to_owned(),
                        s.style.fg,
                        s.style.add_modifier.contains(Modifier::BOLD),
                    )
                })
        };

        let mut v = TuiProgressView::new("Running //...");
        v.apply(&at(0, start("//p:ok")));
        v.apply(&at(10, end("//p:ok", None, None)));
        v.open_dashboard();
        let row = title(&mut v);
        assert_eq!(verdict(&row), None, "running: {}", text(&row));
        v.set_finished(T0 + 2_000);
        let row = title(&mut v);
        assert_eq!(
            verdict(&row),
            Some(("✓ done".to_owned(), Some(Color::Green), true)),
            "{}",
            text(&row)
        );
        assert!(text(&row).contains("✓ done · 2"), "{}", text(&row));

        let mut v = TuiProgressView::new("Running //...");
        for a in ["//p:root", "//p:down"] {
            v.apply(&at(0, start(a)));
        }
        v.apply(&at(10, failed_end("//p:root", None, false)));
        v.apply(&at(11, failed_end("//p:down", Some("//p:root"), false)));
        v.open_dashboard();
        v.set_finished(T0 + 2_000);
        let row = title(&mut v);
        assert_eq!(
            verdict(&row),
            Some(("✗ 2 failed".to_owned(), Some(Color::Red), true)),
            "{}",
            text(&row)
        );
        assert!(
            v.final_line().expect("a summary").contains("2 failed"),
            "the line's own words"
        );

        v.set_stopped();
        let row = title(&mut v);
        assert_eq!(
            verdict(&row),
            Some(("✗ stopped".to_owned(), Some(Color::Red), true)),
            "{}",
            text(&row)
        );
    }

    /// With colour the finished line is painted as the spec's mockup: ✓
    /// green bold, or ✗ red bold with the failed count red bold and its
    /// split dim, the clock bold. Without colour it is `final_line`, which
    /// session viewers compare, with no escape at all.
    #[test]
    fn the_final_line_is_coloured_only_with_colour() {
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, end, start};
        use crossterm::style::Stylize;

        let mut v = TuiProgressView::new("Running //...");
        v.apply(&at(0, start("//p:ok")));
        v.apply(&at(10, end("//p:ok", None, None)));
        v.set_finished(T0 + 2_000);
        let plain = v.final_line().expect("a summary");
        assert_eq!(v.final_line_in(false).as_deref(), Some(plain.as_str()));
        assert!(!plain.contains('\x1b'), "{plain:?}");
        let colored = v.final_line_in(true).expect("a summary");
        assert!(
            colored.starts_with(&format!("{} {} · ", "✓".green().bold(), "2.000s".bold())),
            "{colored:?}"
        );
        assert!(colored.ends_with(" · 0 failed"), "{colored:?}");

        let mut v = TuiProgressView::new("Running //...");
        for a in ["//p:root", "//p:down"] {
            v.apply(&at(0, start(a)));
        }
        v.apply(&at(10, failed_end("//p:root", None, false)));
        v.apply(&at(11, failed_end("//p:down", Some("//p:root"), false)));
        v.set_finished(T0 + 2_000);
        let plain = v.final_line().expect("a summary");
        assert!(!plain.contains('\x1b'), "{plain:?}");
        let colored = v.final_line_in(true).expect("a summary");
        assert!(
            colored.starts_with(&format!("{} {} · ", "✗".red().bold(), "2.000s".bold())),
            "{colored:?}"
        );
        assert!(
            colored.ends_with(&format!(
                "{}{}",
                "2 failed".red().bold(),
                " (1 root, 1 because of it)".dim()
            )),
            "{colored:?}"
        );
        assert_eq!(crate::tui::text::clean_inline(&colored), plain);

        // A stop says so in red.
        v.set_stopped();
        let colored = v.final_line_in(true).expect("a summary");
        assert!(
            colored.starts_with(&format!("{} · ", "✗ stopped".red().bold())),
            "{colored:?}"
        );
    }
}
