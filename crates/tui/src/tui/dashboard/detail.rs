//! What the target screen's Overview reads beside the per-target record:
//! when each phase first started, how long the run waited at admission and
//! how it got its slot, its cache outcome, and the run's shape
//! (driver, resources, declared outputs). Fixed-size per target; a shape is
//! stored once however many runs share it, which is nearly all of them.

use std::collections::{BTreeMap, HashMap};

use hcore::events::{AdmittedKind, BuildEventKind, CacheOutcomeKind};

use super::intern::Id;
use super::model::Record;
use crate::tui::progress::Op;

/// Cache bits: the target's `CacheOutcome`, and whether its hit was rebuilt.
/// Folded from the per-target events only, never the tier events: a target
/// served from the remote is also a local miss, so the tier events are not a
/// target's outcome (`tests/root/cache_tier_gate.rs`).
const LOCAL_HIT: u8 = 1;
const MISS: u8 = 1 << 1;
const REMOTE_HIT: u8 = 1 << 2;
/// A hit that was rebuilt anyway.
const REBUILT: u8 = 1 << 4;
/// The run's result is not cached (`cache = False`).
const NOT_CACHED: u8 = 1 << 5;

/// End bits: how the run ended, and whether it waits on the result lock.
const TIMED_OUT: u8 = 1;
/// Timed out, and heph stopped waiting for it after the grace.
const ABANDONED: u8 = 1 << 1;
/// A result-lock wait is going on.
const LOCK_OPEN: u8 = 1 << 2;

/// One target's Overview facts. 40 bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Detail {
    /// Each op's first start, in ms after [`Details`]' first stamp, plus
    /// one; 0 when it never started. By [`Op::order`].
    op_start: [u32; 5],
    /// The first wait on the result's lock, and the last one's end, on the
    /// same scale as `op_start`; 0 when there was none (or none ended).
    lock_start: u32,
    lock_end: u32,
    /// How long the run waited at admission (`ExecuteStart.queued_ms`).
    queued_ms: u32,
    /// The run's [`Shape`], plus one; 0 before it ran.
    shape: u32,
    admitted: Option<AdmittedKind>,
    cache: u8,
    end: u8,
}

impl Detail {
    /// The run hit its `timeout`: `Some(abandoned)`, true when heph
    /// stopped waiting for it and its process may still be running.
    pub fn timed_out(&self) -> Option<bool> {
        (self.end & TIMED_OUT != 0).then_some(self.end & ABANDONED != 0)
    }

    pub fn admitted(&self) -> Option<AdmittedKind> {
        self.admitted
    }

    pub fn queued_ms(&self) -> u64 {
        u64::from(self.queued_ms)
    }

    /// Whether the target ran (an `ExecuteStart` was seen).
    pub fn ran(&self) -> bool {
        self.shape != 0
    }

    /// The cache row's words: hit (and from which tier) or miss.
    pub fn cache_words(&self) -> Option<&'static str> {
        let c = self.cache;
        if c & REBUILT != 0 {
            Some("hit, rebuilt anyway")
        } else if c & REMOTE_HIT != 0 {
            Some("hit: remote")
        } else if c & LOCAL_HIT != 0 {
            Some("hit: local")
        } else if c & MISS != 0 {
            Some("miss")
        } else if c & NOT_CACHED != 0 {
            Some("not cached (cache = False)")
        } else {
            None
        }
    }
}

/// What a run looked like: its driver, the resources it declared and was
/// granted, and its declared output groups.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Shape {
    pub driver: String,
    pub declared: Option<BTreeMap<String, u64>>,
    pub reserved: Option<BTreeMap<String, u64>>,
    /// Declared output group names, in declaration order; `""` is the
    /// unnamed group.
    pub outputs: Vec<String>,
}

/// One bar of the Overview's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub glyph: char,
    pub label: &'static str,
    /// Unix ms.
    pub start_ms: u64,
    pub end_ms: u64,
    /// Time spent in it (summed when it ran more than once).
    pub ms: u64,
    /// Still going on.
    pub open: bool,
}

/// Every target's [`Detail`], by id, and the shapes they share.
#[derive(Debug, Default)]
pub struct Details {
    /// The first op start seen: the zero of every `op_start`.
    t0: Option<u64>,
    list: Vec<Detail>,
    shapes: Vec<Shape>,
    /// A shape's hash to the indices (plus one) of the shapes with it.
    index: HashMap<u64, Vec<u32>>,
}

/// A [`Shape`] borrowed from the event that carries it.
#[derive(Debug, Clone, Copy)]
struct ShapeRef<'a> {
    driver: &'a str,
    declared: Option<&'a BTreeMap<String, u64>>,
    reserved: Option<&'a BTreeMap<String, u64>>,
    outputs: &'a [String],
}

impl ShapeRef<'_> {
    fn hash(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::hash::DefaultHasher::new();
        (self.driver, self.declared, self.reserved, self.outputs).hash(&mut h);
        h.finish()
    }

    fn is(&self, s: &Shape) -> bool {
        self.driver == s.driver
            && self.declared == s.declared.as_ref()
            && self.reserved == s.reserved.as_ref()
            && self.outputs == s.outputs.as_slice()
    }

    fn stored(self) -> Shape {
        Shape {
            driver: self.driver.to_owned(),
            declared: self.declared.cloned(),
            reserved: self.reserved.cloned(),
            outputs: self.outputs.to_vec(),
        }
    }
}

impl Details {
    /// A new target's (empty) detail; ids are dense, so `push` follows them.
    pub(super) fn push(&mut self) {
        self.list.push(Detail::default());
    }

    pub(super) fn get(&self, id: Id) -> Option<&Detail> {
        self.list.get(id as usize)
    }

    pub(super) fn shape(&self, d: &Detail) -> Option<&Shape> {
        let i = d.shape.checked_sub(1)?;
        self.shapes.get(i as usize)
    }

    /// The address of an event this folds (besides op boundaries).
    pub(super) fn addr_of(kind: &BuildEventKind) -> Option<&str> {
        match kind {
            BuildEventKind::ExecuteStart { addr, .. }
            | BuildEventKind::CacheOutcome { addr, .. }
            | BuildEventKind::CacheHitRebuilt { addr, .. }
            | BuildEventKind::ResultLockWaitStart { addr, .. }
            | BuildEventKind::ResultLockWaitEnd { addr }
            | BuildEventKind::ResultEnd { addr, .. } => Some(addr),
            _ => None,
        }
    }

    /// `at` on the stamps' scale, plus one (0 is "never").
    fn rel(&mut self, at: u64) -> u32 {
        let t0 = *self.t0.get_or_insert(at);
        u32::try_from(at.saturating_sub(t0))
            .unwrap_or(u32::MAX - 1)
            .saturating_add(1)
    }

    /// `op` started on `id` at `at`: kept when it is the op's first start.
    pub(super) fn op_started(&mut self, id: Id, op: Op, at: u64) {
        let rel = self.rel(at);
        if let Some(slot) = self
            .list
            .get_mut(id as usize)
            .and_then(|d| d.op_start.get_mut(usize::from(op.order())))
            && *slot == 0
        {
            *slot = rel;
        }
    }

    pub(super) fn fold(&mut self, id: Id, kind: &BuildEventKind, at: u64) {
        let end_bits = match kind {
            BuildEventKind::ResultLockWaitStart { .. } => {
                let rel = self.rel(at);
                if let Some(d) = self.list.get_mut(id as usize) {
                    if d.lock_start == 0 {
                        d.lock_start = rel;
                    }
                    d.end |= LOCK_OPEN;
                }
                return;
            }
            BuildEventKind::ResultLockWaitEnd { .. } => {
                let rel = self.rel(at);
                if let Some(d) = self.list.get_mut(id as usize)
                    && d.end & LOCK_OPEN != 0
                {
                    d.lock_end = rel;
                    d.end &= !LOCK_OPEN;
                }
                return;
            }
            BuildEventKind::ResultEnd { timed_out, .. } => match timed_out {
                Some(t) if t.abandoned => TIMED_OUT | ABANDONED,
                Some(_) => TIMED_OUT,
                None => 0,
            },
            _ => 0,
        };
        if let BuildEventKind::ResultEnd { .. } = kind {
            let rel = self.rel(at);
            if let Some(d) = self.list.get_mut(id as usize) {
                if d.end & LOCK_OPEN != 0 {
                    d.lock_end = rel;
                }
                d.end = (d.end & !LOCK_OPEN) | end_bits;
            }
            return;
        }
        let bit = match kind {
            BuildEventKind::ExecuteStart {
                driver,
                cache,
                declared,
                reserved,
                queued_ms,
                admitted,
                output_groups,
                ..
            } => {
                let shape = self.intern(ShapeRef {
                    driver,
                    declared: declared.as_ref(),
                    reserved: reserved.as_ref(),
                    outputs: output_groups,
                });
                if let Some(d) = self.list.get_mut(id as usize) {
                    d.shape = shape;
                    d.queued_ms = queued_ms.map_or(0, |q| u32::try_from(q).unwrap_or(u32::MAX));
                    d.admitted = *admitted;
                }
                if *cache { 0 } else { NOT_CACHED }
            }
            BuildEventKind::CacheOutcome { outcome, .. } => match outcome {
                CacheOutcomeKind::LocalHit => LOCAL_HIT,
                CacheOutcomeKind::RemoteHit => REMOTE_HIT,
                CacheOutcomeKind::Miss => MISS,
                CacheOutcomeKind::Unknown => 0,
            },
            BuildEventKind::CacheHitRebuilt { .. } => REBUILT,
            _ => 0,
        };
        if let Some(d) = self.list.get_mut(id as usize) {
            d.cache |= bit;
        }
    }

    /// `shape`'s index plus one, storing it the first time it is seen.
    /// Looked up borrowed, so a start whose shape is known copies nothing,
    /// and a shape is stored once (the index keeps only its hash).
    fn intern(&mut self, shape: ShapeRef<'_>) -> u32 {
        let hash = shape.hash();
        let known = self.index.get(&hash).and_then(|candidates| {
            candidates.iter().copied().find(|&i| {
                i.checked_sub(1)
                    .and_then(|k| self.shapes.get(k as usize))
                    .is_some_and(|s| shape.is(s))
            })
        });
        if let Some(i) = known {
            return i;
        }
        self.shapes.push(shape.stored());
        let i = u32::try_from(self.shapes.len()).unwrap_or(u32::MAX);
        self.index.entry(hash).or_default().push(i);
        i
    }

    /// When `op` first started, in unix ms.
    fn start_of(&self, d: &Detail, op: Op) -> Option<u64> {
        let rel = *d.op_start.get(usize::from(op.order()))?;
        let rel = rel.checked_sub(1)?;
        Some(self.t0?.saturating_add(u64::from(rel)))
    }

    /// The bars of `r`'s timeline, by start: the admission wait before the
    /// run, then each op from its first start for the time it took; the
    /// open op runs to `now_ms`.
    pub(super) fn phases(&self, r: &Record, d: &Detail, now_ms: u64) -> Vec<Phase> {
        let mut out = Vec::new();
        // The wait on the result's lock (another heph building it): from
        // the first wait to the last one's end.
        if let (Some(t0), Some(first)) = (self.t0, d.lock_start.checked_sub(1)) {
            let start = t0.saturating_add(u64::from(first));
            let open = d.end & LOCK_OPEN != 0;
            let end = match d.lock_end.checked_sub(1) {
                _ if open => now_ms.max(start),
                Some(e) => t0.saturating_add(u64::from(e)).max(start),
                None => start,
            };
            out.push(Phase {
                glyph: '⧖',
                label: "lock wait",
                start_ms: start,
                end_ms: end,
                ms: end - start,
                open,
            });
        }
        if let Some(exec) = self.start_of(d, Op::Execute)
            && d.queued_ms > 0
        {
            let q = u64::from(d.queued_ms);
            out.push(Phase {
                glyph: '⧗',
                label: "queued",
                start_ms: exec.saturating_sub(q),
                end_ms: exec,
                ms: q,
                open: false,
            });
        }
        for op in Op::BY_ORDER {
            let Some(start) = self.start_of(d, op) else {
                continue;
            };
            let open = r.op_open() && r.op() == Some(op);
            let live = if open {
                now_ms.saturating_sub(r.op_since_ms)
            } else {
                0
            };
            let ms = r.op_ms(op).saturating_add(live);
            out.push(Phase {
                glyph: op.icon(),
                label: op_label(op),
                start_ms: start,
                end_ms: if open {
                    now_ms.max(start)
                } else {
                    start.saturating_add(ms)
                },
                ms,
                open,
            });
        }
        out.sort_by_key(|p| p.start_ms);
        out
    }
}

/// An op's name on the timeline.
fn op_label(op: Op) -> &'static str {
    match op {
        Op::RemoteCacheRead => "remote read",
        Op::ScratchPrepare => "scratch",
        Op::Execute => "execute",
        Op::LocalCacheWrite => "local write",
        Op::RemoteCacheWrite => "remote write",
    }
}

#[cfg(test)]
mod tests {
    use super::{Detail, Details};
    use hcore::events::BuildEventKind;
    use std::collections::BTreeMap;

    /// Runs of the same shape share one stored shape, found without
    /// copying the event's; a shape differing in any part is its own.
    #[test]
    fn shapes_are_stored_once_and_told_apart() {
        let start = |driver: &str, cpu: u64, outputs: &[&str]| BuildEventKind::ExecuteStart {
            output_groups: outputs.iter().map(|s| (*s).to_owned()).collect(),
            log_not_streamed: false,
            addr: "//p:x".into(),
            driver: driver.into(),
            cache: true,
            declared: Some(BTreeMap::from([("cpu".to_owned(), cpu)])),
            reserved: None,
            queued_ms: None,
            admitted: None,
        };
        let events = [
            start("exec", 1, &["", "bin"]),
            start("exec", 1, &["", "bin"]),
            start("exec", 2, &["", "bin"]),
            start("exec", 1, &["bin", ""]),
            start("bash", 1, &["", "bin"]),
            start("exec", 1, &["", "bin"]),
        ];
        let mut d = Details::default();
        for (id, kind) in (0..).zip(&events) {
            d.push();
            d.fold(id, kind, 1_000);
        }
        let shape = |id: u32| {
            let detail = d.get(id).expect("detail");
            d.shape(detail).expect("shape").clone()
        };
        assert_eq!(d.shapes.len(), 4, "{:#?}", d.shapes);
        assert_eq!(shape(0), shape(1));
        assert_eq!(shape(0), shape(5));
        for other in 2..5 {
            assert_ne!(shape(0), shape(other), "{other}");
        }
        assert_eq!(shape(4).driver, "bash");
        assert_eq!(shape(3).outputs, ["bin", ""]);
    }

    /// The cache row is the target's `CacheOutcome`: a remote hit reads as a
    /// hit though a local miss preceded it, a full miss is one miss, and the
    /// tier events beside them change nothing.
    #[test]
    fn the_cache_row_reads_the_outcome_not_the_tiers() {
        use hcore::events::{CacheHitKind, CacheOutcomeKind};
        let a = || "//p:x".to_owned();
        let outcome = |outcome| BuildEventKind::CacheOutcome {
            addr: a(),
            outcome,
            reason: None,
            stale_host_path: None,
        };
        let cases = [
            (
                vec![
                    BuildEventKind::LocalCacheMiss { addr: a() },
                    BuildEventKind::RemoteCacheHit {
                        addr: a(),
                        cache: Some("bucket".into()),
                        snapshot: false,
                    },
                    outcome(CacheOutcomeKind::RemoteHit),
                ],
                Some("hit: remote"),
            ),
            (
                vec![
                    BuildEventKind::LocalCacheMiss { addr: a() },
                    BuildEventKind::RemoteCacheMiss { addr: a() },
                    outcome(CacheOutcomeKind::Miss),
                ],
                Some("miss"),
            ),
            (
                vec![
                    BuildEventKind::LocalCacheHit { addr: a() },
                    outcome(CacheOutcomeKind::LocalHit),
                    BuildEventKind::CacheHitRebuilt {
                        addr: a(),
                        was: CacheHitKind::Local,
                    },
                ],
                Some("hit, rebuilt anyway"),
            ),
            // Tier events alone are not an outcome.
            (
                vec![
                    BuildEventKind::LocalCacheMiss { addr: a() },
                    BuildEventKind::RemoteCacheMiss { addr: a() },
                ],
                None,
            ),
        ];
        for (events, want) in cases {
            let mut d = Details::default();
            d.push();
            // As the model folds: only the events `addr_of` routes here.
            for kind in events.iter().filter(|k| Details::addr_of(k).is_some()) {
                d.fold(0, kind, 1_000);
            }
            assert_eq!(d.get(0).expect("detail").cache_words(), want, "{events:?}");
        }
    }

    #[test]
    fn detail_size_budget() {
        // With the 56-byte record, under the spec's 100 bytes a target.
        assert!(
            std::mem::size_of::<Detail>() <= 40,
            "Detail is {} bytes",
            std::mem::size_of::<Detail>()
        );
    }
}
