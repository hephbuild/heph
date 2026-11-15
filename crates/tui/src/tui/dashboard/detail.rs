//! What the target screen's Overview reads beside the per-target record:
//! when each phase first started, how long the run waited at admission and
//! how it got its slot, what each cache tier said, and the run's shape
//! (driver, resources, declared outputs). Fixed-size per target; a shape is
//! stored once however many runs share it, which is nearly all of them.

use std::collections::{BTreeMap, HashMap};

use hcore::events::{AdmittedKind, BuildEventKind, CacheOutcomeKind};

use super::intern::Id;
use super::model::Record;
use crate::tui::progress::Op;

/// Cache bits: what each tier said.
const LOCAL_HIT: u8 = 1;
const LOCAL_MISS: u8 = 1 << 1;
const REMOTE_HIT: u8 = 1 << 2;
const REMOTE_MISS: u8 = 1 << 3;
/// A hit that was rebuilt anyway.
const REBUILT: u8 = 1 << 4;
/// The run's result is not cached (`cache = False`).
const NOT_CACHED: u8 = 1 << 5;

/// One target's Overview facts. 32 bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Detail {
    /// Each op's first start, in ms after [`Details`]' first stamp, plus
    /// one; 0 when it never started. By [`Op::order`].
    op_start: [u32; 5],
    /// How long the run waited at admission (`ExecuteStart.queued_ms`).
    queued_ms: u32,
    /// The run's [`Shape`], plus one; 0 before it ran.
    shape: u32,
    admitted: Option<AdmittedKind>,
    cache: u8,
}

impl Detail {
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

    /// The cache row's words: hit or miss, and which tiers were asked.
    pub fn cache_words(&self) -> Option<String> {
        let c = self.cache;
        if c & REBUILT != 0 {
            return Some("hit, rebuilt anyway".to_owned());
        }
        if c & REMOTE_HIT != 0 {
            return Some(
                if c & LOCAL_MISS != 0 {
                    "hit: remote, after a local miss"
                } else {
                    "hit: remote"
                }
                .to_owned(),
            );
        }
        if c & LOCAL_HIT != 0 {
            return Some("hit: local".to_owned());
        }
        let tiers = match (c & LOCAL_MISS != 0, c & REMOTE_MISS != 0) {
            (true, true) => Some("miss: local, then remote"),
            (true, false) => Some("miss: local"),
            (false, true) => Some("miss: remote"),
            (false, false) => None,
        };
        match (tiers, c & NOT_CACHED != 0) {
            (Some(t), true) => Some(format!("{t} · not cached (cache = False)")),
            (Some(t), false) => Some(t.to_owned()),
            (None, true) => Some("not cached (cache = False)".to_owned()),
            (None, false) => None,
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
    index: HashMap<Shape, u32>,
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
            | BuildEventKind::LocalCacheHit { addr }
            | BuildEventKind::LocalCacheMiss { addr }
            | BuildEventKind::RemoteCacheHit { addr }
            | BuildEventKind::RemoteCacheMiss { addr }
            | BuildEventKind::CacheOutcome { addr, .. }
            | BuildEventKind::CacheHitRebuilt { addr, .. } => Some(addr),
            _ => None,
        }
    }

    /// `op` started on `id` at `at`: kept when it is the op's first start.
    pub(super) fn op_started(&mut self, id: Id, op: Op, at: u64) {
        let t0 = *self.t0.get_or_insert(at);
        let rel = u32::try_from(at.saturating_sub(t0))
            .unwrap_or(u32::MAX - 1)
            .saturating_add(1);
        if let Some(slot) = self
            .list
            .get_mut(id as usize)
            .and_then(|d| d.op_start.get_mut(usize::from(op.order())))
            && *slot == 0
        {
            *slot = rel;
        }
    }

    pub(super) fn fold(&mut self, id: Id, kind: &BuildEventKind, _at: u64) {
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
                let shape = self.intern(Shape {
                    driver: driver.clone(),
                    declared: declared.clone(),
                    reserved: reserved.clone(),
                    outputs: output_groups.clone(),
                });
                if let Some(d) = self.list.get_mut(id as usize) {
                    d.shape = shape;
                    d.queued_ms = queued_ms.map_or(0, |q| u32::try_from(q).unwrap_or(u32::MAX));
                    d.admitted = *admitted;
                }
                if *cache { 0 } else { NOT_CACHED }
            }
            BuildEventKind::LocalCacheHit { .. }
            | BuildEventKind::CacheOutcome {
                outcome: CacheOutcomeKind::LocalHit,
                ..
            } => LOCAL_HIT,
            BuildEventKind::LocalCacheMiss { .. } => LOCAL_MISS,
            BuildEventKind::RemoteCacheHit { .. }
            | BuildEventKind::CacheOutcome {
                outcome: CacheOutcomeKind::RemoteHit,
                ..
            } => REMOTE_HIT,
            BuildEventKind::RemoteCacheMiss { .. } => REMOTE_MISS,
            BuildEventKind::CacheHitRebuilt { .. } => REBUILT,
            _ => 0,
        };
        if let Some(d) = self.list.get_mut(id as usize) {
            d.cache |= bit;
        }
    }

    /// `shape`'s index plus one, storing it the first time it is seen.
    fn intern(&mut self, shape: Shape) -> u32 {
        if let Some(&i) = self.index.get(&shape) {
            return i;
        }
        self.shapes.push(shape.clone());
        let i = u32::try_from(self.shapes.len()).unwrap_or(u32::MAX);
        self.index.insert(shape, i);
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
    use super::Detail;

    #[test]
    fn detail_size_budget() {
        // With the 56-byte record, under the spec's 100 bytes a target.
        assert!(
            std::mem::size_of::<Detail>() <= 32,
            "Detail is {} bytes",
            std::mem::size_of::<Detail>()
        );
    }
}
