//! Event builders for the line's and the spotlight's tests.

use hcore::events::{BuildEvent, BuildEventKind, YieldingToData};

use super::BuildState;

/// The stream's first instant.
pub(crate) const T0: u64 = 1_700_000_000_000;

/// `kind` at `T0 + ms`.
pub(crate) fn at(ms: u64, kind: BuildEventKind) -> BuildEvent {
    BuildEvent {
        at_unix_ms: T0 + ms,
        kind,
    }
}

pub(crate) fn start(addr: &str) -> BuildEventKind {
    BuildEventKind::ResultStart { addr: addr.into() }
}

pub(crate) fn end(addr: &str, error: Option<&str>, upstream_of: Option<&str>) -> BuildEventKind {
    BuildEventKind::ResultEnd {
        left_behind: None,
        cancelled: false,
        addr: addr.into(),
        error: error.map(Into::into),
        upstream_of: upstream_of.map(Into::into),
        exit_status: None,
        log_tail: None,
        timed_out: None,
    }
}

pub(crate) fn exec(addr: &str) -> BuildEventKind {
    BuildEventKind::ExecuteStart {
        output_groups: Vec::new(),
        addr: addr.into(),
        driver: "exec".into(),
        cache: true,
        declared: None,
        reserved: None,
        queued_ms: None,
        admitted: None,
    }
}

pub(super) fn lock(addr: &str, holder_pid: Option<u32>) -> BuildEventKind {
    BuildEventKind::ResultLockWaitStart {
        addr: addr.into(),
        holder_pid,
        in_use_by_readers: false,
    }
}

pub(super) fn lock_end(addr: &str) -> BuildEventKind {
    BuildEventKind::ResultLockWaitEnd { addr: addr.into() }
}

pub(super) fn scratch(addr: &str, cache: &str) -> BuildEventKind {
    BuildEventKind::ScratchLockWaitStart {
        addr: addr.into(),
        scratch: cache.into(),
        access: "exclusive".into(),
        holder_pid: None,
    }
}

/// An admission wait on `pool`, with its sandbox removals and the waiter it
/// yields to, when those apply.
pub(super) fn admission(
    addr: &str,
    pool: &str,
    removals_pending: Option<u64>,
    yielding_to: Option<YieldingToData>,
) -> BuildEventKind {
    BuildEventKind::AdmissionWaitStart {
        addr: addr.into(),
        pool: pool.into(),
        need: 2,
        free: 1,
        ahead: 3,
        removals_pending,
        yielding_to,
    }
}

/// `events` folded into a fresh state.
pub(super) fn fold(events: &[BuildEvent]) -> BuildState {
    let mut s = BuildState::new();
    for e in events {
        s.apply(e);
    }
    s
}
