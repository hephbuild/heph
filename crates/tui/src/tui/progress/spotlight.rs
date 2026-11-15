//! The line's spotlight: the one target worth knowing about right now.
//!
//! Each frame takes the first rule with something to show, from what
//! [`BuildState`] already folds:
//!
//! 1. a pending approval (the view's, not the fold's: it is not an event);
//! 2. the newest root failure — the first one, until a newer one replaces it;
//! 3. the longest wait: admission, result lock or scratch lock;
//! 4. the longest-running target, past [`LONG_RUNNING_THRESHOLD_MS`].
//!
//! Rules 3 and 4 read the front of a set kept ordered by start time as each
//! event is folded, so a frame does constant work however many targets wait
//! (I7). A target rules 3 and 4 picked keeps the spotlight for at least
//! [`HOLD_MS`] unless a higher rule takes over or it stops waiting or running;
//! the hold is decided while folding, from event times alone, so every view of
//! one event stream — the run's own, an attached one that started late —
//! spotlights the same target at the same moment.

use std::borrow::Cow;
use std::collections::BTreeSet;

use ratatui::style::{Color, Modifier, Style};

use super::{BuildState, LONG_RUNNING_THRESHOLD_MS, LockHolder};
use crate::tui::text::clean_inline;

/// The least time rules 3 and 4 keep a target in the spotlight.
pub const HOLD_MS: u64 = 2_000;

/// Widest a root failure's reason is kept; the line cuts it further.
const REASON_MAX: usize = 120;

/// What the spotlight orders by start time, kept as events fold.
#[derive(Debug, Default)]
pub(super) struct SpotIndex {
    /// `(since, addr)` of every admission wait.
    pub(super) admission: BTreeSet<(u64, String)>,
    /// `(since, addr)` of every result-lock wait.
    pub(super) lock: BTreeSet<(u64, String)>,
    /// `(since, consumer addr)` of every scratch-lock wait.
    pub(super) scratch: BTreeSet<(u64, String)>,
    /// Scratch waits per cache, `(since, consumer addr)`: the group a
    /// scratch spotlight names and its oldest wait.
    pub(super) by_cache: std::collections::HashMap<String, BTreeSet<(u64, String)>>,
    /// `(active op start, addr)` of every target with an op open.
    pub(super) running: BTreeSet<(u64, String)>,
    /// The root failure rule 2 shows.
    pub(super) root_failure: Option<RootFailure>,
    /// What rules 3 and 4 picked, and since when.
    hold: Option<Held>,
}

/// A failure no dependency caused, as the spotlight shows it.
#[derive(Debug, Clone)]
pub(super) struct RootFailure {
    pub(super) at_ms: u64,
    pub(super) addr: String,
    /// Why, in a few words: `exit 101`, `timed out 10m`, or the error's first
    /// line. Control bytes already removed.
    pub(super) reason: String,
}

impl RootFailure {
    /// The spotlight's reason for a failed `ResultEnd`: its exit, its
    /// timeout, or else its error's first line, with heph's home redacted
    /// (I9).
    pub(super) fn reason(
        error: &str,
        exit_status: Option<&str>,
        timed_out: Option<&hcore::events::TimedOutData>,
        redact: &crate::tui::text::Redact,
    ) -> String {
        if let Some(t) = timed_out {
            return format!(
                "timed out {}",
                hcore::units::human_duration(std::time::Duration::from_millis(t.timeout_ms))
            );
        }
        let text = match exit_status {
            Some(s) => match s.strip_prefix("exit status: ") {
                Some(code) => Cow::Owned(format!("exit {code}")),
                None => Cow::Borrowed(s),
            },
            None => redact.apply(error.lines().next().unwrap_or("")),
        };
        let clean = clean_inline(&text);
        crate::tui::text::cut_right(&clean, REASON_MAX).into_owned()
    }

    /// Whether this failure, at `at_ms` on `addr`, replaces `self`: it is
    /// newer, or as new and first by address.
    pub(super) fn is_replaced_by(&self, at_ms: u64, addr: &str) -> bool {
        at_ms > self.at_ms || (at_ms == self.at_ms && addr < self.addr.as_str())
    }
}

/// What rules 3 and 4 hold.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpotKey {
    Admission(String),
    Lock(String),
    /// By cache: scratch waits are grouped.
    Scratch(String),
    Run(String),
}

/// A [`SpotKey`] borrowed from the fold, so a frame allocates no key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpotRef<'a> {
    Admission(&'a str),
    Lock(&'a str),
    Scratch(&'a str),
    Run(&'a str),
}

impl SpotRef<'_> {
    /// The rule: lower is higher priority.
    fn rule(self) -> u8 {
        match self {
            Self::Admission(_) | Self::Lock(_) | Self::Scratch(_) => 3,
            Self::Run(_) => 4,
        }
    }

    fn owned(self) -> SpotKey {
        match self {
            Self::Admission(a) => SpotKey::Admission(a.to_owned()),
            Self::Lock(a) => SpotKey::Lock(a.to_owned()),
            Self::Scratch(c) => SpotKey::Scratch(c.to_owned()),
            Self::Run(a) => SpotKey::Run(a.to_owned()),
        }
    }
}

impl SpotKey {
    fn as_ref(&self) -> SpotRef<'_> {
        match self {
            Self::Admission(a) => SpotRef::Admission(a),
            Self::Lock(a) => SpotRef::Lock(a),
            Self::Scratch(c) => SpotRef::Scratch(c),
            Self::Run(a) => SpotRef::Run(a),
        }
    }
}

#[derive(Debug, Clone)]
struct Held {
    key: SpotKey,
    since_ms: u64,
}

/// What the spotlight shows, ready for the line to lay out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spot {
    /// One glyph: `✦` approval, `✗` failure, `⧗` wait, `▶` run.
    pub icon: &'static str,
    pub style: Style,
    /// What it is about: an address, or a cache. Control bytes removed. The
    /// line shortens this, from the left, before it drops the spotlight.
    pub name: String,
    /// The rest: `exit 101`, `7 queued 31s`, `42s`. May be empty.
    pub detail: String,
}

impl Spot {
    /// A spotlit pending approval: `addr`, with `N pending` when `pending`
    /// counts more than this one.
    pub fn approval(addr: &str, pending: u64) -> Self {
        Self {
            icon: "✦",
            style: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            name: clean_inline(addr).into_owned(),
            detail: if pending > 1 {
                format!("{pending} pending")
            } else {
                String::new()
            },
        }
    }

    /// The whole text, as the line draws it when nothing is shortened.
    pub fn text(&self) -> String {
        if self.detail.is_empty() {
            format!("{} {}", self.icon, self.name)
        } else {
            format!("{} {} {}", self.icon, self.name, self.detail)
        }
    }
}

fn wait_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn elapsed(ms: u64) -> String {
    hcore::units::human_duration(std::time::Duration::from_millis(ms))
}

#[cfg(test)]
thread_local! {
    /// Ordered-set fronts a pick has read, for `spotlight_cost_and_hold`.
    pub(super) static FRONTS_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The front of an ordered wait set.
fn front(set: &BTreeSet<(u64, String)>) -> Option<&(u64, String)> {
    #[cfg(test)]
    FRONTS_READ.with(|n| n.set(n.get() + 1));
    set.first()
}

/// The dashboard's blocked rows read the same wait maps, in the same words:
/// an admission wait's are `admission_wait_text`'s, a lock's name the
/// holder's session as the spotlight does.
impl crate::tui::dashboard::Waits for BuildState {
    fn wait_words(&self, addr: &str, now_ms: u64) -> Option<crate::tui::dashboard::WaitWords> {
        use crate::tui::dashboard::WaitWords;
        if let Some(w) = self.admission_waits.get(addr) {
            let text = self.admission_text(addr, now_ms)?;
            // `<addr> waits for <pool> <need> (<free>, <queue>, <waited>)[ —
            // cleanup …]`: PHASE is `waits <pool> <need>` (`waits sandbox`,
            // whose need is always one slot), LAST LINE the rest.
            let rest = text.strip_prefix(addr).unwrap_or(&text).trim_start();
            let rest = rest.strip_prefix("waits for ").unwrap_or(rest);
            let (head, detail) = rest.split_once(" (").unwrap_or((rest, ""));
            let phase = if w.pool == hcore::capacity::SANDBOX {
                format!("waits {}", w.pool)
            } else {
                format!("waits {head}")
            };
            return Some(WaitWords {
                phase,
                detail: format!("({detail}"),
                waited_ms: now_ms.saturating_sub(w.since_ms),
            });
        }
        if let Some((holder, since)) = self.lock_waits.get(addr) {
            let (phase, detail) = match holder {
                LockHolder::Pid(pid) => match self.holders.get(*pid) {
                    Some(id) => (
                        format!("locked: {id}"),
                        format!("held by session {id} (pid {pid})"),
                    ),
                    None => ("locked".to_owned(), format!("held by pid {pid}")),
                },
                LockHolder::Readers => ("locked".to_owned(), "in use by readers".to_owned()),
                LockHolder::Unknown => ("locked".to_owned(), "holder unknown".to_owned()),
            };
            return Some(WaitWords {
                phase,
                detail,
                waited_ms: now_ms.saturating_sub(*since),
            });
        }
        let w = self.scratch_waits.get(addr)?;
        let detail = match w.holder_pid {
            Some(pid) => format!("held by {}", self.holder(pid)),
            None => "queued".to_owned(),
        };
        Some(WaitWords {
            phase: format!("waits {}", w.scratch),
            detail,
            waited_ms: now_ms.saturating_sub(w.since_ms),
        })
    }
}

impl BuildState {
    /// `addr`'s admission wait in the shared formatter's words, waited up
    /// to `now_ms`.
    fn admission_text(&self, addr: &str, now_ms: u64) -> Option<String> {
        let w = self.admission_waits.get(addr)?;
        let waited = elapsed(now_ms.saturating_sub(w.since_ms));
        Some(hcore::events::admission_wait_text(
            addr,
            &w.pool,
            self.unit_of(&w.pool),
            w.need,
            w.free,
            w.ahead,
            w.yielding_to
                .as_ref()
                .map(|to| (to, self.unit_of(&to.pool))),
            w.removals_pending,
            &waited,
        ))
    }

    /// Rule 2: the root failure on show, if any.
    pub(super) fn spot_failure(&self) -> Option<Spot> {
        let f = self.spot.root_failure.as_ref()?;
        Some(Spot {
            icon: "✗",
            style: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            name: clean_inline(&f.addr).into_owned(),
            detail: f.reason.clone(),
        })
    }

    /// Rules 3 and 4 without the hold: the oldest wait, else the target whose
    /// open op started first once it is past the long-running threshold. Ties
    /// on start break on address.
    fn spot_raw(&self, now_ms: u64) -> Option<SpotRef<'_>> {
        let i = &self.spot;
        let admission = front(&i.admission).map(|(t, a)| (*t, a.as_str(), SpotRef::Admission(a)));
        let lock = front(&i.lock).map(|(t, a)| (*t, a.as_str(), SpotRef::Lock(a)));
        let scratch = front(&i.scratch).and_then(|(t, a)| {
            let w = self.scratch_waits.get(a)?;
            Some((*t, a.as_str(), SpotRef::Scratch(w.scratch.as_str())))
        });
        let wait = [admission, lock, scratch]
            .into_iter()
            .flatten()
            .min_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)));
        if let Some((_, _, key)) = wait {
            return Some(key);
        }
        let (start, addr) = front(&i.running)?;
        (now_ms.saturating_sub(*start) > LONG_RUNNING_THRESHOLD_MS).then_some(SpotRef::Run(addr))
    }

    /// Whether `key` still waits or runs: a held target that stopped hands
    /// over at once.
    fn spot_alive(&self, key: SpotRef<'_>) -> bool {
        match key {
            SpotRef::Admission(a) => self.admission_waits.contains_key(a),
            SpotRef::Lock(a) => self.lock_waits.contains_key(a),
            SpotRef::Scratch(c) => self.spot.by_cache.get(c).is_some_and(|s| !s.is_empty()),
            SpotRef::Run(a) => self.open_ops.contains(a),
        }
    }

    /// Rules 3 and 4 with the hold, at `now_ms`.
    fn spot_pick(&self, now_ms: u64) -> Option<SpotRef<'_>> {
        let raw = self.spot_raw(now_ms);
        let held = self
            .spot
            .hold
            .as_ref()
            .filter(|h| self.spot_alive(h.key.as_ref()));
        match (held, raw) {
            (Some(h), Some(r)) if r.rule() < h.key.as_ref().rule() => Some(r),
            (Some(h), _) if now_ms < h.since_ms.saturating_add(HOLD_MS) => Some(h.key.as_ref()),
            (_, r) => r,
        }
    }

    /// Carry the hold across one folded event at `at_ms`. Called once per
    /// event, after it is applied: a change of pick starts a new hold at the
    /// event's time, so the hold never depends on which frames were drawn.
    pub(super) fn fold_spotlight(&mut self, at_ms: u64) {
        let pick = self.spot_pick(at_ms);
        let same = match (pick, &self.spot.hold) {
            (Some(p), Some(h)) => p == h.key.as_ref(),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.spot.hold = pick.map(|p| Held {
                key: p.owned(),
                since_ms: at_ms,
            });
        }
    }

    /// Rules 2 to 4 at `now_ms`, as the line shows them. Rule 1 is the view's:
    /// an approval is not an event.
    pub fn spotlight(&self, now_ms: u64) -> Option<Spot> {
        if let Some(f) = self.spot_failure() {
            return Some(f);
        }
        match self.spot_pick(now_ms)? {
            SpotRef::Admission(addr) => {
                let text = self.admission_text(addr, now_ms)?;
                // The shared formatter's words, after the address it starts
                // with, which the line may shorten on its own.
                let detail = text.strip_prefix(addr).unwrap_or(&text).trim_start();
                Some(Spot {
                    icon: "⧗",
                    style: wait_style(),
                    name: clean_inline(addr).into_owned(),
                    detail: clean_inline(detail).into_owned(),
                })
            }
            SpotRef::Lock(addr) => {
                let (holder, since) = self.lock_waits.get(addr)?;
                let waited = elapsed(now_ms.saturating_sub(*since));
                let by = match holder {
                    LockHolder::Pid(pid) => match self.holders.get(*pid) {
                        Some(id) => format!("locked by session {id}"),
                        None => format!("locked by pid {pid}"),
                    },
                    LockHolder::Readers => "locked, in use".to_owned(),
                    LockHolder::Unknown => "locked, holder unknown".to_owned(),
                };
                Some(Spot {
                    icon: "⧗",
                    style: wait_style(),
                    name: clean_inline(addr).into_owned(),
                    detail: format!("{by} {waited}"),
                })
            }
            SpotRef::Scratch(cache) => {
                let waits = self.spot.by_cache.get(cache)?;
                let (since, _) = waits.first()?;
                let waited = elapsed(now_ms.saturating_sub(*since));
                // Any waiter that named a holder speaks for the group, as the
                // box's scratch row does; only the oldest is read here.
                let holder = waits
                    .first()
                    .and_then(|(_, a)| self.scratch_waits.get(a))
                    .and_then(|w| w.holder_pid)
                    .map(|pid| match self.holders.get(pid) {
                        Some(id) => format!(", held by session {id}"),
                        None => format!(", held by pid {pid}"),
                    })
                    .unwrap_or_default();
                Some(Spot {
                    icon: "⧗",
                    style: wait_style(),
                    name: clean_inline(cache).into_owned(),
                    detail: format!("{} queued{holder} {waited}", waits.len()),
                })
            }
            SpotRef::Run(addr) => {
                let start = self.ops.get(addr)?.active?.1;
                Some(Spot {
                    icon: "▶",
                    style: Style::default().fg(Color::Cyan),
                    name: clean_inline(addr).into_owned(),
                    detail: elapsed(now_ms.saturating_sub(start)),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hcore::capacity::Unit;
    use hcore::events::{BuildEvent, BuildEventKind, YieldingToData};

    use super::super::test_events::{
        T0, admission, at, end, exec, fold, lock, lock_end, scratch, start,
    };
    use super::*;
    use crate::tui::app::TUIAppView;
    use crate::tui::approval::ApprovalCenter;
    use crate::tui::progress::TuiProgressView;
    use crate::tui::session::{HolderNames, Session, SessionRoot, SessionStart};

    fn shown(s: &BuildState, ms: u64) -> Option<String> {
        s.spotlight(T0 + ms).map(|s| s.text())
    }

    /// The events behind rules 2 to 4; rule 1 is the view's.
    fn rule_events(rule: usize) -> Vec<BuildEvent> {
        match rule {
            2 => vec![
                at(0, start("//r:fail")),
                at(10, end("//r:fail", Some("boom"), None)),
            ],
            3 => vec![at(0, start("//r:wait")), at(20, lock("//r:wait", None))],
            4 => vec![at(0, start("//r:run")), at(30, exec("//r:run"))],
            _ => Vec::new(),
        }
    }

    const ICONS: [&str; 4] = ["✦", "✗", "⧗", "▶"];

    /// Approval > root failure > longest wait > longest run, for each pair,
    /// as the line shows it.
    #[test]
    fn spotlight_priority() {
        for hi in 1..=4 {
            for lo in hi + 1..=4 {
                let center = ApprovalCenter::new();
                let mut v = TuiProgressView::new("L").with_approval(center.clone());
                for e in rule_events(hi).into_iter().chain(rule_events(lo)) {
                    v.apply(&e);
                }
                let build = center.build();
                let _ticket =
                    (hi == 1).then(|| center.request(&build, "//r:gate".into(), Vec::new()));
                let lines = v.render("⠋", T0 + 20_000, 300, 24);
                let line: String = lines
                    .last()
                    .expect("the line")
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect();
                let (want, not) = (ICONS[hi - 1], ICONS[lo - 1]);
                assert!(
                    line.contains(want) && !line.contains(not),
                    "rule {hi} over rule {lo}: {line}"
                );
            }
        }
    }

    fn cache_write(addr: &str) -> BuildEventKind {
        BuildEventKind::LocalCacheWriteStart { addr: addr.into() }
    }

    #[test]
    fn spotlight_cost_and_hold() {
        // Rule 3 reads the fronts of the ordered sets: as many at 100k
        // waiters as at 10.
        let reads = |n: u64| {
            let events: Vec<_> = (0..n)
                .map(|i| at(i, admission(&format!("//w:{i}"), "cpu", None, None)))
                .collect();
            let s = fold(&events);
            FRONTS_READ.with(|c| c.set(0));
            let spot = s.spotlight(T0 + 200_000).expect("a wait");
            assert_eq!(spot.name, "//w:0", "the oldest wait");
            FRONTS_READ.with(std::cell::Cell::get)
        };
        let few = reads(10);
        assert!(few <= 4, "{few} fronts read");
        assert_eq!(reads(100_000), few, "constant work at 100k waiters");

        // The hold, from event times alone. `r1` is picked when the wait
        // ends at 9s; at 10s it moves on to its cache write, so its op now
        // starts after `r2`'s, and the hold keeps it until 11s.
        let events = vec![
            at(0, start("//h:r1")),
            at(0, exec("//h:r1")),
            at(100, start("//h:w")),
            at(100, lock("//h:w", None)),
            at(1_000, start("//h:r2")),
            at(1_000, exec("//h:r2")),
            at(9_000, lock_end("//h:w")),
            at(10_000, cache_write("//h:r1")),
        ];
        // A view that ran from the start, drawing frames as it went...
        let mut live = BuildState::new();
        for e in &events {
            live.apply(e);
            for frame in [0, 400, 800] {
                drop(live.spotlight(e.at_unix_ms + frame));
            }
        }
        // ...and one that attached at 10.6s and folded it all at once.
        let late = fold(&events);
        for s in [&live, &late] {
            assert_eq!(shown(s, 10_600).as_deref(), Some("▶ //h:r1 0s"));
            assert_eq!(shown(s, 11_100).as_deref(), Some("▶ //h:r2 10s"));
        }
        // A held target that finishes inside its hold hands over at once.
        let mut ended = events.clone();
        ended.push(at(10_300, end("//h:r1", None, None)));
        assert_eq!(shown(&fold(&ended), 10_400).as_deref(), Some("▶ //h:r2 9s"));

        // A root failure stays until a newer root replaces it; a failure
        // caused by a dependency is never spotlit.
        let mut failures = vec![
            at(0, start("//f:a")),
            at(1_000, end("//f:a", Some("first\nmore"), None)),
            at(1_500, start("//f:b")),
            at(2_000, end("//f:b", Some("dep failed"), Some("//f:a"))),
        ];
        assert_eq!(
            shown(&fold(&failures), 60_000).as_deref(),
            Some("✗ //f:a first")
        );
        failures.push(at(3_000, end("//f:c", Some("second"), None)));
        assert_eq!(
            shown(&fold(&failures), 60_000).as_deref(),
            Some("✗ //f:c second")
        );

        // No wait event, no wait: the engine sends one past its notice
        // threshold. A target that has run for less than the long-running
        // threshold is not spotlit either, and an empty build shows nothing.
        let started = fold(&[at(0, start("//q:a")), at(0, exec("//q:a"))]);
        assert_eq!(shown(&started, LONG_RUNNING_THRESHOLD_MS - 1), None);
        assert_eq!(shown(&BuildState::new(), 60_000), None);

        // Scratch waits group by cache.
        let grouped = fold(&[
            at(0, scratch("//s:a", "//build:gocache")),
            at(1_000, scratch("//s:b", "//build:gocache")),
        ]);
        assert_eq!(
            shown(&grouped, 31_000).as_deref(),
            Some("⧗ //build:gocache 2 queued 31s")
        );

        // A lock held by another session names it.
        let root = tempfile::tempdir().expect("tempdir");
        let sessions = SessionRoot::new(root.path());
        let holder = Session::open(&sessions, SessionStart::default()).expect("holder");
        let own = Session::open(&sessions, SessionStart::default()).expect("own");
        let names = HolderNames::new(Some(sessions.clone()), Some(own.id()));
        names.resolve_blocking(std::process::id());
        let mut locked = BuildState::new();
        locked.name_lock_holders(names);
        locked.apply(&at(0, lock("//l:a", Some(std::process::id()))));
        assert_eq!(
            shown(&locked, 39_000),
            Some(format!("⧗ //l:a locked by session {} 39s", holder.id()))
        );
        holder.finish(Some(0), false);
        own.finish(Some(0), false);

        // "N pending" counts the requests queued with the one on show.
        assert_eq!(Spot::approval("//a:g", 3).text(), "✦ //a:g 3 pending");
        assert_eq!(Spot::approval("//a:g", 1).text(), "✦ //a:g");
    }

    /// The spotlight's words for an admission wait are the shared
    /// formatter's, after the address; a waiter that yields shows no queue
    /// position, since its event carries none worth showing.
    #[test]
    fn wait_text_is_the_shared_formatter_spotlight() {
        let yielding = YieldingToData {
            pool: "memory".into(),
            need: 1 << 30,
        };
        for (pool, removals, yields) in [
            ("cpu", None, None),
            ("cpu", None, Some(yielding)),
            ("sandbox", Some(2), None),
        ] {
            let s = fold(&[at(0, admission("//w:a", pool, removals, yields.clone()))]);
            let spot = s.spotlight(T0 + 41_000).expect("a wait");
            let expected = hcore::events::admission_wait_text(
                "//w:a",
                pool,
                Unit::default_for(pool),
                2,
                1,
                3,
                yields.as_ref().map(|y| (y, Unit::default_for(&y.pool))),
                removals,
                "41s",
            );
            assert_eq!(format!("{} {}", spot.name, spot.detail), expected);
            assert_eq!(spot.icon, "⧗");
            if yields.is_some() {
                assert!(!spot.detail.contains("ahead"), "{}", spot.detail);
            }
        }
    }
}
