//! The line: one row that says how the build is going.
//!
//! Left to right: spinner and clock, the counts, the CPU the running targets
//! reserved, a standing approval decision, the spotlight, and the hint for
//! the dashboard right-aligned. It never wraps (I2). As the terminal
//! narrows, segments go in a fixed order — the subject the matcher is
//! resolving, then the spotlight (shortened first), then the CPU figure, then
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

/// How the run stands: still going (with this frame's spinner), or over.
#[derive(Debug, Clone, Copy)]
pub(super) enum Status<'a> {
    Running(&'a str),
    Finished { failed: bool },
}

/// What a build's line counts. Strings where the box's header already
/// formats them (`~` while the matcher streams).
#[derive(Debug)]
pub(super) struct BuildCounts {
    pub(super) done: String,
    /// `None` before any `Matched` event: `{done} done`.
    pub(super) total: Option<String>,
    pub(super) cached: usize,
    /// Admission, result-lock and scratch-lock waits together.
    pub(super) waiting: usize,
    pub(super) failed: usize,
    /// `cpu 11/12`: what the running targets reserved, of the pool.
    pub(super) cpu: Option<String>,
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
    /// What the run is resolving, while the matcher streams. Dropped first.
    pub(super) resolving: Option<Cow<'a, str>>,
    pub(super) body: Body<'a>,
    pub(super) standing: Option<ApprovalStanding>,
    pub(super) spot: Option<Spot>,
    /// Whether the command has a dashboard for `tab` to open.
    pub(super) hint: bool,
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
        Status::Finished { failed: false } => ("✓".to_owned(), Style::default().fg(Color::Green)),
        Status::Finished { failed: true } => (
            "✗".to_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let clock = Span::styled(
        m.clock.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    );
    Seg {
        width: width(&glyph) + 1 + width(&m.clock),
        spans: vec![Span::styled(glyph, style), Span::raw(" "), clock],
    }
}

/// The required segments of `tier`, after the head: counts, CPU, standing.
fn counts(m: &LineModel<'_>, tier: Tier) -> Vec<Seg> {
    let running = matches!(m.status, Status::Running(_));
    let mut segs = Vec::with_capacity(6);
    match &m.body {
        // Cut to fit by the caller: a gc segment is one piece of text.
        Body::Text(_) => {}
        Body::Build(c) => {
            let done = match (&c.total, tier) {
                (Some(t), Tier::Full | Tier::NoSpot) => format!("{} / {t} done", c.done),
                (Some(t), _) => format!("{}/{t}", c.done),
                (None, Tier::Full | Tier::NoSpot) => format!("{} done", c.done),
                (None, _) => c.done.clone(),
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

    let optional = running && matches!(tier, Tier::Full | Tier::NoSpot);
    let resolving = m
        .resolving
        .as_deref()
        .filter(|_| optional)
        .map(|r| Seg::styled(clean_inline(r).into_owned(), dim()));
    let spot_candidate = m.spot.as_ref().filter(|_| running);
    let spot_seg = spot_candidate.filter(|_| tier == Tier::Full);

    // Everything, when it fits; else the subject goes first, then the
    // spotlight is shortened, then it goes too. A tier without the spotlight
    // shows the subject only when there is no spotlight to drop before it,
    // so nothing comes back as the line narrows.
    let whole_spot = spot_seg.and_then(|s| spot(s, usize::MAX));
    let extra = resolving.as_ref().map_or(0, |r| r.width + sep_w)
        + whole_spot.as_ref().map_or(0, |s| s.width + sep_w);
    let (resolving, spot_seg) = if base + extra <= room && whole_spot.is_some() {
        (resolving, whole_spot)
    } else if spot_candidate.is_none() {
        (resolving.filter(|r| base + r.width + sep_w <= room), None)
    } else {
        let fitted = spot_seg.and_then(|s| spot(s, room.checked_sub(base + sep_w)?));
        (None, fitted)
    };
    if let Some(r) = resolving {
        segs.insert(1, r);
    }
    segs.extend(spot_seg);

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
            let done = match &c.total {
                Some(t) => format!("{}/{t}", c.done),
                None => c.done.clone(),
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
            resolving: None,
            body: Body::Build(BuildCounts {
                done: "128".to_owned(),
                total: Some("412".to_owned()),
                cached: 96,
                waiting: 3,
                failed: 0,
                cpu: Some("cpu 11/12".to_owned()),
            }),
            standing: None,
            spot,
            hint: true,
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

    #[test]
    fn finished_line_is_the_summary() {
        let mut m = model(Some(run_spot("//a:b")));
        m.status = Status::Finished { failed: true };
        m.standing = Some(ApprovalStanding::Approve);
        if let Body::Build(c) = &mut m.body {
            c.failed = 3;
        }
        assert_eq!(
            text(&render(&m, 120)),
            "✗ 1m05s · 128 / 412 done · 96 cached · 3 failed"
        );
    }

    #[test]
    fn resolving_goes_before_the_spotlight_shortens() {
        let mut m = model(Some(run_spot("//go/cmd/heph-govet:build")));
        m.resolving = Some(Cow::Borrowed("Running //..."));
        let wide = text(&render(&m, 160));
        assert!(wide.starts_with("⠹ 1m05s · Running //... · 128"), "{wide}");
        assert!(wide.contains("▶ //go/cmd/heph-govet:build 42s"), "{wide}");
        let narrow = text(&render(&m, 105));
        assert!(!narrow.contains("Running"), "{narrow}");
        assert!(narrow.contains("▶ "), "{narrow}");
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
            resolving: None,
            body: Body::Text(Cow::Borrowed(
                "12 targets · 40 revisions · 1.2 GiB freed · cache.db 4 MiB",
            )),
            standing: None,
            spot: None,
            hint: false,
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
                detail:
                    "waits for cpu 2 (1 free, 3 ahead, 41s) — cleanup is behind: 2 removals pending"
                        .to_owned(),
            },
        ];
        for spot in spots {
            let mut m = model(Some(spot.clone()));
            m.resolving = Some(Cow::Borrowed("Running //漢字/..."));
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

            // Segments drop in the stated order: the subject, the spotlight
            // (shortened first), the CPU figure, then the long words, then the
            // cached count.
            let resolving = drops_at(&m, |t| t.contains("Running"));
            let shortened = drops_at(&m, |t| t.contains(&spot.text()));
            let spotlit = drops_at(&m, |t| t.contains(spot.icon));
            let cpu = drops_at(&m, |t| t.contains("cpu 11/12"));
            let words = drops_at(&m, |t| t.contains("3 waiting"));
            let cached = drops_at(&m, |t| t.contains("96 cached"));
            assert!(
                resolving >= shortened
                    && shortened >= spotlit
                    && spotlit > cpu
                    && cpu >= words
                    && words > cached,
                "{resolving} {shortened} {spotlit} {cpu} {words} {cached}"
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
    /// label, the approval prompt — reaches the terminal with no control
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
                addr: "//p:fail".into(),
                error: Some(format!("exec failed\n{SANDBOX}/run.sh: exit status: 101")),
                upstream_of: None,
                exit_status: Some("exit status: 101".into()),
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
                content: "drop the table".into(),
                path: format!("{HOME}/approval/notices/plan.txt"),
                hash: hcore::approval_notice::notice_hash(b"drop the table"),
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
                addr: addr.into(),
                error: Some(error),
                upstream_of: upstream_of.map(str::to_owned),
                exit_status: None,
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
}
