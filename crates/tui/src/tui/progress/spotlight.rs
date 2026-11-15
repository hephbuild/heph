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
    /// The spotlight's reason for a failed `ResultEnd`.
    pub(super) fn reason(
        error: &str,
        exit_status: Option<&str>,
        timed_out: Option<&hcore::events::TimedOutData>,
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
            None => Cow::Borrowed(error.lines().next().unwrap_or("")),
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

impl BuildState {
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
                let w = self.admission_waits.get(addr)?;
                let waited = elapsed(now_ms.saturating_sub(w.since_ms));
                let text = hcore::events::admission_wait_text(
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
                );
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
