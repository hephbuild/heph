//! Shared state bridging an approval-gated target (engine side) and the
//! interactive TUI (render + keyboard). The engine's approval handler enqueues a
//! request and awaits a one-shot decision; the [`TuiProgressView`] renders the
//! pending prompt on the main view, and the interactive backend resolves it from
//! a keypress.
//!
//! Each request gets a sequence number when it is queued, and a decision names
//! the number it decides ([`ApprovalCenter::respond`]), so a key decides the
//! request on screen even when it is no longer the front of the queue. A
//! number no longer queued decides nothing. A queued request whose receiver
//! closed (its target was cancelled) is dropped from the queue.
//!
//! A standing decision ([`ApprovalCenter::set_standing`], Y/N) approves or
//! rejects every request of one build pending now and every one that build
//! queues later. It belongs to the build ([`ApprovalBuild`]), not the center,
//! and ends when the build's handle drops. Setting it, deciding the queue under
//! it, and the check in [`ApprovalCenter::request`] are one critical section of
//! the center's mutex, so no request slips between them.
//!
//! Every decision logs one line ([`ApprovalDecision::line`]) once the lock is
//! released, a request decided by a standing decision included: the run's
//! record of what was approved.
//!
//! [`TuiProgressView`]: crate::tui::progress::TuiProgressView

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

pub use crate::tui::session::ApprovalStanding;

/// One rendered notice for display: the input group name, its text, the
/// legacy on-disk path an older attached viewer shows, and the content hash it
/// is stored under (`hcore::approval_notice`), which is what a session records
/// for a viewer to read it back by.
#[derive(Clone, Debug)]
pub struct ApprovalNoticeView {
    pub name: String,
    pub content: String,
    pub path: String,
    pub hash: String,
}

struct Pending {
    seq: u64,
    build: u64,
    /// When the build started, for the decision line's `at`.
    started: Instant,
    addr: String,
    notices: Vec<ApprovalNoticeView>,
    responder: oneshot::Sender<bool>,
}

#[derive(Default)]
struct Inner {
    /// Gated targets awaiting a decision, oldest first.
    queue: VecDeque<Pending>,
    /// The request whose notice body is expanded (Enter toggles), by number:
    /// the next request at the front starts collapsed.
    expanded: Option<u64>,
    /// The last sequence number handed out; numbers start at 1.
    last_seq: u64,
    /// The last build number handed out.
    last_build: u64,
    /// Standing decisions by build. A center serves one build in practice,
    /// so a list beats a map.
    standing: Vec<(u64, ApprovalStanding)>,
}

impl Inner {
    /// Drop every queued request whose receiver is gone: its target was
    /// cancelled, so no decision can reach it and none must be shown.
    fn prune(&mut self) {
        self.queue.retain(|p| !p.responder.is_closed());
    }

    fn standing_of(&self, build: u64) -> Option<ApprovalStanding> {
        self.standing
            .iter()
            .find_map(|&(b, s)| (b == build).then_some(s))
    }

    fn next_seq(&mut self) -> u64 {
        self.last_seq += 1;
        self.last_seq
    }
}

/// Cloneable handle to the pending-approval queue, shared between the engine's
/// approval handler and the TUI renderer/backend.
#[derive(Clone, Default)]
pub struct ApprovalCenter {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for ApprovalCenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalCenter").finish_non_exhaustive()
    }
}

/// One build's scope on an [`ApprovalCenter`]: the requests it queues, and the
/// standing decision set on them, which ends when this handle drops (the build
/// is over).
#[derive(Debug)]
pub struct ApprovalBuild {
    center: ApprovalCenter,
    id: u64,
    started: Instant,
}

impl Drop for ApprovalBuild {
    fn drop(&mut self) {
        self.center.lock().standing.retain(|&(b, _)| b != self.id);
    }
}

/// A queued request: its number, and the receiver resolved with the decision.
/// A request decided as it arrived (a standing decision) has its receiver
/// already resolved.
#[derive(Debug)]
pub struct ApprovalTicket {
    pub seq: u64,
    pub rx: oneshot::Receiver<bool>,
}

/// What decided a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecidedBy {
    /// y or n, on that request.
    Key,
    /// A standing Y or N.
    Standing,
}

/// One decided request, as its decision line reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub seq: u64,
    pub addr: String,
    pub approved: bool,
    pub by: DecidedBy,
    /// Since the build started.
    pub at: Duration,
    /// How many notices the target declared.
    pub notices: usize,
    /// The notices read before deciding, by name. Always empty for a
    /// standing decision.
    pub read: Vec<String>,
}

impl ApprovalDecision {
    /// The scrollback line recording this decision, e.g.
    /// `✓ approved //x · by standing approval (Y) after 2m16s · notices not
    /// read`. The duration is how long the prompt waited for the decision.
    pub fn line(&self) -> String {
        let (mark, verb) = if self.approved {
            ("✓", "approved")
        } else {
            ("✗", "rejected")
        };
        let by = match (self.by, self.approved) {
            (DecidedBy::Key, true) => "key (y)",
            (DecidedBy::Key, false) => "key (n)",
            (DecidedBy::Standing, true) => "standing approval (Y)",
            (DecidedBy::Standing, false) => "standing rejection (N)",
        };
        let mut line = format!(
            "{mark} {verb} {} · by {by} after {}",
            self.addr,
            hcore::units::human_duration(self.at)
        );
        if self.notices > 0 {
            if self.read.is_empty() {
                line.push_str(" · notices not read");
            } else {
                line.push_str(" · read ");
                line.push_str(&self.read.join(", "));
            }
        }
        line
    }

    fn of(p: &Pending, approved: bool, by: DecidedBy, read: Vec<String>) -> Self {
        Self {
            seq: p.seq,
            addr: p.addr.clone(),
            approved,
            by,
            at: p.started.elapsed(),
            notices: p.notices.len(),
            read,
        }
    }
}

/// Log each decision's line. Called with the center's lock released.
fn record(decisions: &[ApprovalDecision]) {
    for d in decisions {
        tracing::info!("{}", d.line());
    }
}

/// A render-ready snapshot of one pending request.
#[derive(Clone, Debug)]
pub struct ApprovalView {
    pub seq: u64,
    pub addr: String,
    pub notices: Vec<ApprovalNoticeView>,
    pub expanded: bool,
    /// How many further targets are queued behind this one.
    pub queued_behind: usize,
}

impl ApprovalCenter {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a panic while holding it; the queue is plain data,
        // so recover the guard rather than cascading the panic into the renderer.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lock, with requests whose target was cancelled dropped.
    fn lock_pruned(&self) -> std::sync::MutexGuard<'_, Inner> {
        let mut inner = self.lock();
        inner.prune();
        inner
    }

    /// Open a build's scope: what [`Self::request`] queues under, and what a
    /// standing decision covers.
    pub fn build(&self) -> ApprovalBuild {
        let id = {
            let mut inner = self.lock();
            inner.last_build += 1;
            inner.last_build
        };
        ApprovalBuild {
            center: self.clone(),
            id,
            started: Instant::now(),
        }
    }

    /// Engine side: queue a gated target of `build` and return its number and
    /// the receiver resolved when it is decided. Under a standing decision of
    /// `build` it is decided here instead, its line logged, and the receiver
    /// comes back resolved. Dropping the receiver (request cancelled) drops the
    /// request from the queue.
    pub fn request(
        &self,
        build: &ApprovalBuild,
        addr: String,
        notices: Vec<ApprovalNoticeView>,
    ) -> ApprovalTicket {
        let (tx, rx) = oneshot::channel();
        let (seq, decided) = {
            let mut inner = self.lock_pruned();
            let seq = inner.next_seq();
            let pending = Pending {
                seq,
                build: build.id,
                started: build.started,
                addr,
                notices,
                responder: tx,
            };
            let decided = match inner.standing_of(build.id) {
                Some(standing) => {
                    let approved = standing == ApprovalStanding::Approve;
                    let decision =
                        ApprovalDecision::of(&pending, approved, DecidedBy::Standing, Vec::new());
                    // The receiver is in hand, so the send cannot fail.
                    let _sent = pending.responder.send(approved);
                    Some(decision)
                }
                None => {
                    inner.queue.push_back(pending);
                    None
                }
            };
            (seq, decided)
        };
        if let Some(d) = decided {
            record(std::slice::from_ref(&d));
        }
        ApprovalTicket { seq, rx }
    }

    /// Whether a prompt is currently awaiting a decision.
    pub fn is_active(&self) -> bool {
        !self.lock_pruned().queue.is_empty()
    }

    /// Snapshot the front request for rendering, or `None` when idle.
    pub fn current(&self) -> Option<ApprovalView> {
        let inner = self.lock_pruned();
        let front = inner.queue.front()?;
        Some(ApprovalView {
            seq: front.seq,
            addr: front.addr.clone(),
            notices: front.notices.clone(),
            expanded: inner.expanded == Some(front.seq),
            queued_behind: inner.queue.len().saturating_sub(1),
        })
    }

    /// Every pending request, oldest first.
    pub fn pending(&self) -> Vec<ApprovalView> {
        let inner = self.lock_pruned();
        let len = inner.queue.len();
        inner
            .queue
            .iter()
            .enumerate()
            .map(|(i, p)| ApprovalView {
                seq: p.seq,
                addr: p.addr.clone(),
                notices: p.notices.clone(),
                expanded: inner.expanded == Some(p.seq),
                queued_behind: len - i - 1,
            })
            .collect()
    }

    /// The standing decision in force, if any build has one.
    pub fn standing(&self) -> Option<ApprovalStanding> {
        self.lock().standing.first().map(|&(_, s)| s)
    }

    /// The front request as a session records it: no notice content, no
    /// responder. `None` when idle, without allocating.
    pub fn prompt(&self) -> Option<crate::tui::session::ApprovalPrompt> {
        let inner = self.lock_pruned();
        let front = inner.queue.front()?;
        Some(crate::tui::session::ApprovalPrompt {
            addr: front.addr.clone(),
            queued_behind: u64::try_from(inner.queue.len().saturating_sub(1)).unwrap_or(u64::MAX),
            notices: front
                .notices
                .iter()
                .map(|n| crate::tui::session::ApprovalPromptNotice {
                    name: n.name.clone(),
                    path: n.path.clone(),
                    hash: Some(n.hash.clone()),
                })
                .collect(),
        })
    }

    /// Expand/collapse the front request's notice body (Enter).
    pub fn toggle_expanded(&self) {
        let mut inner = self.lock_pruned();
        let Some(front) = inner.queue.front().map(|p| p.seq) else {
            return;
        };
        inner.expanded = if inner.expanded == Some(front) {
            None
        } else {
            Some(front)
        };
    }

    /// Decide request `seq`, having read the notices named in `read`, and log
    /// its line. Nothing happens, and `None` comes back, when `seq` is no
    /// longer queued: already decided, or its target cancelled.
    pub fn respond(&self, seq: u64, approve: bool, read: Vec<String>) -> Option<ApprovalDecision> {
        let decision = {
            let mut inner = self.lock_pruned();
            let at = inner.queue.iter().position(|p| p.seq == seq)?;
            let pending = inner.queue.remove(at)?;
            let decision = ApprovalDecision::of(&pending, approve, DecidedBy::Key, read);
            // A receiver dropped since the prune: cancelled, nothing decided.
            pending.responder.send(approve).ok()?;
            decision
        };
        record(std::slice::from_ref(&decision));
        Some(decision)
    }

    /// Y/N: approve (or reject) every pending request of the build request
    /// `seq` belongs to, and every request that build queues later, logging a
    /// line for each. Returns the requests decided now. Nothing happens when
    /// `seq` is no longer queued.
    pub fn set_standing(&self, seq: u64, standing: ApprovalStanding) -> Vec<ApprovalDecision> {
        let approved = standing == ApprovalStanding::Approve;
        let decided = {
            let mut inner = self.lock_pruned();
            let Some(build) = inner.queue.iter().find(|p| p.seq == seq).map(|p| p.build) else {
                return Vec::new();
            };
            match inner.standing.iter_mut().find(|(b, _)| *b == build) {
                Some(entry) => entry.1 = standing,
                None => inner.standing.push((build, standing)),
            }
            let mut decided = Vec::new();
            let mut kept = VecDeque::with_capacity(inner.queue.len());
            for pending in std::mem::take(&mut inner.queue) {
                if pending.build != build {
                    kept.push_back(pending);
                    continue;
                }
                let decision =
                    ApprovalDecision::of(&pending, approved, DecidedBy::Standing, Vec::new());
                if pending.responder.send(approved).is_ok() {
                    decided.push(decision);
                }
            }
            inner.queue = kept;
            decided
        };
        record(&decided);
        decided
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(name: &str) -> ApprovalNoticeView {
        ApprovalNoticeView {
            name: name.to_string(),
            content: format!("{name} body"),
            path: format!("/home/approval/{name}.txt"),
            hash: hcore::approval_notice::notice_hash(format!("{name} body").as_bytes()),
        }
    }

    #[tokio::test]
    async fn request_then_approve_resolves_receiver() {
        let center = ApprovalCenter::new();
        let build = center.build();
        let t = center.request(&build, "//a:b".to_string(), vec![notice("plan")]);
        assert!(center.is_active());
        let view = center.current().expect("active");
        assert_eq!(view.addr, "//a:b");
        assert_eq!(view.seq, t.seq);
        assert_eq!(view.notices.len(), 1);
        assert!(!view.expanded);

        center.toggle_expanded();
        assert!(center.current().expect("active").expanded);

        let d = center
            .respond(t.seq, true, vec!["plan".into()])
            .expect("decided");
        assert!(!center.is_active());
        assert!(t.rx.await.expect("decision"));
        assert!(
            d.line().starts_with("✓ approved //a:b · by key (y) after "),
            "{}",
            d.line()
        );
        assert!(d.line().ends_with(" · read plan"), "{}", d.line());
    }

    #[tokio::test]
    async fn reject_sends_false_and_queue_advances() {
        let center = ApprovalCenter::new();
        let build = center.build();
        let t1 = center.request(&build, "//a:1".to_string(), vec![]);
        let t2 = center.request(&build, "//a:2".to_string(), vec![]);
        assert_eq!(center.current().expect("active").queued_behind, 1);

        center.respond(t1.seq, false, Vec::new());
        assert!(!t1.rx.await.expect("decision"));
        // Second prompt is now active.
        assert_eq!(center.current().expect("active").addr, "//a:2");
        center.respond(t2.seq, true, Vec::new());
        assert!(t2.rx.await.expect("decision"));
        assert!(!center.is_active());
    }

    /// Feed-level half of the spec's `y_decides_the_request_on_screen`: with
    /// the view on the second of three requests, y decides that one, not the
    /// front of the queue; and a number already decided decides nothing. The
    /// screen clause (opened from the Approvals tab) is unit 6's
    /// `approval_view_decides_and_returns`.
    #[test]
    fn y_decides_the_request_on_screen() {
        let center = ApprovalCenter::new();
        let build = center.build();
        let mut t1 = center.request(&build, "//a:1".into(), vec![]);
        let mut t2 = center.request(&build, "//a:2".into(), vec![]);
        let mut t3 = center.request(&build, "//a:3".into(), vec![]);
        let on_screen = center.pending()[1].clone();
        assert_eq!((on_screen.seq, on_screen.addr.as_str()), (t2.seq, "//a:2"));

        let d = center
            .respond(on_screen.seq, true, Vec::new())
            .expect("decided");
        assert_eq!(d.addr, "//a:2");
        assert_eq!(t2.rx.try_recv(), Ok(true));
        assert!(t1.rx.try_recv().is_err(), "the front is still pending");
        assert!(t3.rx.try_recv().is_err());
        let left: Vec<_> = center.pending().into_iter().map(|v| v.addr).collect();
        assert_eq!(left, ["//a:1", "//a:3"]);

        // Pressed again on a number already decided: nothing happens, and
        // in particular the front is not decided in its place.
        assert_eq!(center.respond(on_screen.seq, false, Vec::new()), None);
        assert_eq!(center.pending().len(), 2);
        assert!(t1.rx.try_recv().is_err());
    }

    /// A queued request whose target was cancelled (receiver dropped) leaves
    /// the queue, and answering its number does nothing.
    #[test]
    fn cancelled_request_is_dropped() {
        let center = ApprovalCenter::new();
        let build = center.build();
        let t1 = center.request(&build, "//a:1".into(), vec![]);
        let t2 = center.request(&build, "//a:2".into(), vec![]);
        let seq1 = t1.seq;
        drop(t1);
        assert_eq!(center.current().expect("active").addr, "//a:2");
        assert_eq!(center.pending().len(), 1);
        assert_eq!(center.respond(seq1, true, Vec::new()), None);
        drop(t2);
        assert!(!center.is_active());
        assert!(center.prompt().is_none());
    }

    /// Feed-level half of the spec's `standing_yes_covers_later_requests`.
    /// The screen clauses (no prompt row or view, the indicator on the line,
    /// the title row and `heph session show`) are unit 6's
    /// `approval_view_decides_and_returns`; an attached viewer holds no
    /// center, so it cannot set one (`only_the_live_view_acts_on_the_run`).
    #[test]
    fn standing_yes_covers_later_requests() {
        let center = ApprovalCenter::new();
        let build = center.build();
        let other = center.build();
        let mut t1 = center.request(&build, "//a:1".into(), vec![notice("plan")]);
        let mut t2 = center.request(&build, "//a:2".into(), vec![]);
        let mut foreign = center.request(&other, "//b:1".into(), vec![]);
        assert_eq!(center.standing(), None, "no Y/N: nothing stands");

        let decided = center.set_standing(t2.seq, ApprovalStanding::Approve);
        let addrs: Vec<_> = decided.iter().map(|d| d.addr.as_str()).collect();
        assert_eq!(
            addrs,
            ["//a:1", "//a:2"],
            "every pending request of the build, once each"
        );
        assert_eq!(t1.rx.try_recv(), Ok(true));
        assert_eq!(t2.rx.try_recv(), Ok(true));
        assert!(
            decided[0]
                .line()
                .starts_with("✓ approved //a:1 · by standing approval (Y) after ")
                && decided[0].line().ends_with(" · notices not read"),
            "{}",
            decided[0].line()
        );
        assert_eq!(center.standing(), Some(ApprovalStanding::Approve));

        // Scoped to the build: another build's request still waits.
        assert!(foreign.rx.try_recv().is_err());
        assert_eq!(center.pending().len(), 1);

        // A later request of the build is decided as it arrives, never queued.
        let mut t3 = center.request(&build, "//a:3".into(), vec![]);
        assert_eq!(t3.rx.try_recv(), Ok(true));
        assert_eq!(center.pending().len(), 1, "only the other build's request");
        // Its number decides nothing now.
        assert_eq!(center.respond(t3.seq, false, Vec::new()), None);

        // N on the other build rejects its requests, now and later.
        let rejected = center.set_standing(foreign.seq, ApprovalStanding::Reject);
        assert_eq!(rejected.len(), 1);
        assert!(
            rejected[0]
                .line()
                .starts_with("✗ rejected //b:1 · by standing rejection (N) after "),
            "{}",
            rejected[0].line()
        );
        assert_eq!(foreign.rx.try_recv(), Ok(false));
        let mut later = center.request(&other, "//b:2".into(), vec![]);
        assert_eq!(later.rx.try_recv(), Ok(false));

        // The standing decision ends with its build.
        drop(build);
        assert_eq!(center.standing(), Some(ApprovalStanding::Reject));
        drop(other);
        assert_eq!(center.standing(), None);
        let fresh = center.build();
        let mut t = center.request(&fresh, "//c:1".into(), vec![]);
        assert!(t.rx.try_recv().is_err(), "a new build prompts again");
    }

    /// A request racing Y is decided exactly once: either queued before Y and
    /// decided by it, or decided as it arrives — never left pending.
    #[test]
    fn standing_races_request_without_losing_one() {
        const N: usize = 64;
        for _ in 0..20 {
            let center = ApprovalCenter::new();
            let build = Arc::new(center.build());
            let first = center.request(&build, "//a:0".into(), vec![]);
            let barrier = Arc::new(std::sync::Barrier::new(N + 1));
            let handles: Vec<_> = (1..=N)
                .map(|i| {
                    let (center, build, barrier) = (center.clone(), build.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        center.request(&build, format!("//a:{i}"), vec![])
                    })
                })
                .collect();
            barrier.wait();
            let by_y = center.set_standing(first.seq, ApprovalStanding::Approve);
            let tickets: Vec<_> = handles
                .into_iter()
                .map(|h| h.join().expect("join"))
                .collect();
            for mut t in tickets.into_iter().chain([first]) {
                assert_eq!(
                    t.rx.try_recv(),
                    Ok(true),
                    "request {} left undecided",
                    t.seq
                );
            }
            assert!(!center.is_active());
            assert!(!by_y.is_empty() && by_y.len() <= N + 1);
        }
    }
}
