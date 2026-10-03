//! Keep-going discovery: what a selector walk could not resolve, and why.
//!
//! A selector walk ([`Engine::query`](crate::engine::Engine::query)) meets six
//! places where resolving a candidate can fail: a provider's package listing, a
//! package's probe, a provider's `list` for a package, and the spec or def of a
//! candidate the matcher cannot decide from its address — plus the spec of a
//! matched candidate in `query_spec`. Under [`Discovery::Complete`] the first
//! such failure ends the walk. Under [`Discovery::KeepGoing`] the walk records it
//! in a [`Gaps`] sink and carries on, so the command acts on everything that did
//! resolve and then reports the selection as incomplete.
//!
//! Users see one noun for this, "incomplete selection". "Gap" is the code's word.

use crate::engine::error::CancelledError;
use crate::engine::request_state::RequestState;
use hcore::hmemoizer::downcast_chain_ref;
use parking_lot::Mutex;
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What a selector walk does with a candidate it cannot resolve.
///
/// [`Engine::query`](crate::engine::Engine::query) and `query_spec` take it as a
/// plain argument with no default: a truncated set is harmless to `heph run`
/// and corrupting to `heph tool gen-gitignore`, so every caller states which it
/// is. The `Default` exists only so `ResultOptions` keeps one, and it is
/// [`Complete`](Self::Complete) — an API caller that never asked for keep-going
/// must not get a silently partial batch.
#[derive(Debug, Clone, Default)]
pub enum Discovery {
    /// Fail the walk on the first candidate that cannot be resolved. For walks
    /// whose output must be the whole set: anything that writes a file from it,
    /// and `--fail-fast`.
    #[default]
    Complete,
    /// Skip it, record it in the sink, and carry on.
    ///
    /// The sink rides in the argument rather than on the request because some
    /// commands walk on a private request that is dropped before they finish
    /// (`tool clean`, `tool scratch`): skips recorded there would vanish, and the
    /// command would exit 0 on a partial set.
    KeepGoing(Arc<Gaps>),
}

impl Discovery {
    /// [`KeepGoing`](Self::KeepGoing) into `gaps`, unless the command was asked
    /// to stop at the first failure.
    ///
    /// `--fail-fast` is decided here, at the call site, and never read from the
    /// request: `Engine::new_state()` defaults to `fail_fast = true`, so coupling
    /// the two would switch keep-going off for every command that uses it.
    pub fn keep_going_unless(fail_fast: bool, gaps: &Arc<Gaps>) -> Self {
        if fail_fast {
            Self::Complete
        } else {
            Self::KeepGoing(Arc::clone(gaps))
        }
    }

    /// The sink, under [`KeepGoing`](Self::KeepGoing).
    pub(crate) fn gaps(&self) -> Option<&Arc<Gaps>> {
        match self {
            Self::Complete => None,
            Self::KeepGoing(gaps) => Some(gaps),
        }
    }
}

/// Where in the walk a candidate could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// A provider's `list_packages`: every package of that provider is lost.
    Packages,
    /// A package's probe: every candidate in that package is lost.
    Probe,
    /// A provider's `list` for one package: that provider's candidates there.
    List,
    /// A candidate's spec, needed to check it against the selector or to hand
    /// it to the caller.
    Spec,
    /// A candidate's def, needed to check it against the selector.
    Def,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Packages => "packages",
            Self::Probe => "probe",
            Self::List => "list",
            Self::Spec => "spec",
            Self::Def => "def",
        }
    }

    /// What one skip at this stage is a skip *of*, for a report line.
    pub fn noun(self, count: usize) -> &'static str {
        match (self, count == 1) {
            (Self::Packages, true) => "package listing",
            (Self::Packages, false) => "package listings",
            (Self::Probe | Self::List, true) => "package",
            (Self::Probe | Self::List, false) => "packages",
            (Self::Spec | Self::Def, true) => "target",
            (Self::Spec | Self::Def, false) => "targets",
        }
    }

    /// What went wrong at this stage, in words a user knows — `as_str` is the
    /// machine key and stays internal vocabulary. Reads after
    /// [`noun`](Self::noun) for either count: "3 targets could not be loaded".
    pub fn phrase(self) -> &'static str {
        match self {
            Self::Packages => "failed",
            Self::Probe => "could not be read",
            Self::List => "could not be listed",
            Self::Spec => "could not be loaded",
            Self::Def => "could not be resolved",
        }
    }
}

/// How many example scopes a group keeps. The smallest ones, so the report is
/// the same on every run whatever order the walk completed in.
pub const EXAMPLES_PER_GROUP: usize = 5;

/// How long one example's cause line may get before it is cut. A cause is a
/// flattened error chain, and a plugin error can carry a whole subprocess
/// stderr. The full text is in the failure boxes when a target run caused the
/// skip, in the `debug` log for every skip, and in front of the user again
/// under `--fail-fast`.
const CAUSE_MAX_CHARS: usize = 160;

/// Everything a keep-going walk skipped, aggregated as it is recorded.
///
/// What it renders is bounded: one group per (stage, provider) holding an
/// exact count and the [`EXAMPLES_PER_GROUP`] smallest examples with one cause
/// line each. The dedup set is one small key per skipped scope — linear in what
/// was skipped, tens of bytes each — and no error object is kept, because every
/// Go candidate in a workspace can be skipped at once.
///
/// One sink may serve several walks — `heph validate` runs three on one request
/// — and a scope skipped by more than one of them counts once.
#[derive(Debug)]
pub struct Gaps {
    selector: String,
    inner: Mutex<GapsInner>,
}

#[derive(Debug, Default)]
struct GapsInner {
    seen: FxHashSet<(Stage, String, String)>,
    groups: BTreeMap<(Stage, String), Group>,
}

#[derive(Debug, Default)]
struct Group {
    count: usize,
    examples: BTreeMap<String, String>,
}

impl Gaps {
    /// A sink for a walk over `selector`, the user's own spelling of what they
    /// asked for (the report quotes it back).
    pub fn new(selector: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            selector: selector.into(),
            inner: Mutex::new(GapsInner::default()),
        })
    }

    pub fn selector(&self) -> &str {
        &self.selector
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().seen.is_empty()
    }

    /// Record one skipped scope — a provider, a package or an addr — at `stage`.
    ///
    /// `provider` may be empty when the failure carries no provider (the
    /// report then names only the stage).
    pub fn record(&self, stage: Stage, provider: &str, scope: String, err: &anyhow::Error) {
        let mut inner = self.inner.lock();
        let key = (stage, provider.to_string());
        if !inner.seen.insert((key.0, key.1.clone(), scope.clone())) {
            return;
        }
        // The full error, once per skip. The report keeps one line of it, and
        // a skip no target caused (a BUILD syntax error, a provider's `get`)
        // never reaches the failure boxes, so this is where the rest lives.
        // Formatted only when debug is on.
        tracing::debug!(
            stage = stage.as_str(),
            provider,
            scope,
            error = %format!("{err:#}"),
            "selection incomplete: skipped"
        );
        let group = inner.groups.entry(key).or_default();
        group.count += 1;
        let keep = group.examples.len() < EXAMPLES_PER_GROUP
            || group
                .examples
                .last_key_value()
                .is_some_and(|(largest, _)| scope < *largest);
        if !keep {
            return;
        }
        let cause = cause_line(err, &scope);
        group.examples.insert(scope, cause);
        if group.examples.len() > EXAMPLES_PER_GROUP {
            group.examples.pop_last();
        }
    }

    /// A snapshot of what was skipped, groups in (stage, provider) order.
    pub fn report(&self) -> GapReport {
        let inner = self.inner.lock();
        GapReport {
            selector: self.selector.clone(),
            skipped: inner.seen.len(),
            groups: inner
                .groups
                .iter()
                .map(|((stage, provider), group)| GapGroup {
                    stage: *stage,
                    provider: provider.clone(),
                    count: group.count,
                    examples: group
                        .examples
                        .iter()
                        .map(|(scope, cause)| GapExample {
                            scope: scope.clone(),
                            cause: cause.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// What [`Gaps`] held when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapReport {
    pub selector: String,
    /// Exact number of distinct skipped scopes, across every group.
    pub skipped: usize,
    pub groups: Vec<GapGroup>,
}

impl GapReport {
    /// The report as the one `SelectionIncomplete` event a command emits.
    pub fn to_event(&self) -> crate::engine::event::BuildEventKind {
        use hcore::events::{SelectionGap, SelectionGapExample};
        crate::engine::event::BuildEventKind::SelectionIncomplete {
            skipped: self.skipped,
            groups: self
                .groups
                .iter()
                .map(|g| SelectionGap {
                    stage: g.stage.as_str().to_string(),
                    provider: g.provider.clone(),
                    count: g.count,
                    examples: g
                        .examples
                        .iter()
                        .map(|e| SelectionGapExample {
                            scope: e.scope.clone(),
                            cause: e.cause.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapGroup {
    pub stage: Stage,
    /// Empty when the failure did not say which provider it came from.
    pub provider: String,
    /// Exact number of distinct scopes skipped in this group.
    pub count: usize,
    /// At most [`EXAMPLES_PER_GROUP`], the smallest scopes, sorted.
    pub examples: Vec<GapExample>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapExample {
    /// The addr, package (`//pkg`) or provider listing that was skipped.
    pub scope: String,
    pub cause: String,
}

/// One line of cause for a report: the error chain after the last frame that
/// names `scope` (those only restate the report's first column), as its first
/// non-blank line, cut at [`CAUSE_MAX_CHARS`].
fn cause_line(err: &anyhow::Error, scope: &str) -> String {
    // Split the flattened text rather than walking `chain()`: an error that
    // went through a memoizer cell comes back as one frame already flattened
    // with `{:#}`, so its frames are only recoverable from the `: ` separators.
    let joined = format!("{err:#}");
    let segments: Vec<&str> = joined.split(": ").collect();
    // Through the last frame naming the scope — but never the final frame,
    // which is the root cause however it reads. `get_def: //p:b: resolving
    // target `//p:b`: go list: exit status 1` keeps `go list: exit status 1`.
    let last = segments.len().saturating_sub(1);
    let start = segments
        .iter()
        .take(last)
        .rposition(|s| names(s, scope))
        .map_or(0, |i| i + 1);
    let rest = segments.get(start..).unwrap_or_default().join(": ");
    // Plugin errors carry subprocess output; keep its bytes off the terminal.
    let line: String = rest
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    // Cut from the front: a chain reads outermost context first, so the root
    // cause — the part worth a line — is at the end.
    let chars = line.chars().count();
    if chars <= CAUSE_MAX_CHARS {
        return line;
    }
    let tail: String = line.chars().skip(chars - CAUSE_MAX_CHARS).collect();
    format!("…{tail}")
}

/// Whether `frame` names `scope` as a whole token: not `//p:ab` for `//p:a`,
/// and not `//a/b` for `//a`, so a frame naming something longer — a
/// dependency, a path under the package — is kept as part of the cause.
fn names(frame: &str, scope: &str) -> bool {
    let continues = |c: char| c.is_alphanumeric() || "_-./@=,:".contains(c);
    frame.match_indices(scope).any(|(i, _)| {
        // `match_indices` yields char boundaries, so both `get`s succeed.
        let before = frame.get(..i).and_then(|s| s.chars().next_back());
        let after = frame.get(i + scope.len()..).and_then(|s| s.chars().next());
        !before.is_some_and(continues) && !after.is_some_and(continues)
    })
}

/// True when `err` is the walk being cancelled rather than a candidate failing.
///
/// The request token first: a cancelled `get` with two dependencies in flight
/// returns a `MultiError`, which the typed downcast does not look inside. A
/// cancellation is never a gap — it ends the walk.
pub(crate) fn is_cancellation(rs: &RequestState, err: &anyhow::Error) -> bool {
    rs.ctoken().is_cancelled() || downcast_chain_ref::<CancelledError>(err).is_some()
}

/// Which provider a failure came from, carried as typed context so a keep-going
/// walk can group its skips by provider.
///
/// A plugin flattens a host error to a string at the cdylib boundary, so the
/// failing *target* behind a provider's error is not recoverable here — but the
/// engine always knows which provider's `get` or `probe` it was calling, and
/// says so with this.
#[derive(Debug)]
pub struct InProvider {
    pub provider: String,
    /// The frame's message, e.g. ``resolving target `//a:b` ``.
    pub what: String,
}

impl std::fmt::Display for InProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for InProvider {}

/// The provider an error names through [`InProvider`], or `""`.
pub(crate) fn provider_of(err: &anyhow::Error) -> &str {
    downcast_chain_ref::<InProvider>(err).map_or("", |p| p.provider.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{msg}")
    }

    #[test]
    fn a_scope_counts_once_whichever_walk_records_it() {
        let gaps = Gaps::new("//...");
        for _ in 0..3 {
            gaps.record(Stage::Spec, "go", "//a:x".into(), &err("boom"));
        }
        gaps.record(Stage::Spec, "go", "//a:y".into(), &err("boom"));
        let report = gaps.report();
        assert_eq!(report.skipped, 2);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].count, 2);
    }

    #[test]
    fn examples_are_the_smallest_whatever_the_insert_order() {
        let forward = Gaps::new("//...");
        let backward = Gaps::new("//...");
        let scopes: Vec<String> = (0..20).map(|i| format!("//p:t{i:02}")).collect();
        for s in &scopes {
            forward.record(Stage::Spec, "go", s.clone(), &err("boom"));
        }
        for s in scopes.iter().rev() {
            backward.record(Stage::Spec, "go", s.clone(), &err("boom"));
        }
        let (f, b) = (forward.report(), backward.report());
        assert_eq!(f, b);
        let kept: Vec<&str> = f.groups[0]
            .examples
            .iter()
            .map(|e| e.scope.as_str())
            .collect();
        assert_eq!(
            kept,
            ["//p:t00", "//p:t01", "//p:t02", "//p:t03", "//p:t04"]
        );
        assert_eq!(f.groups[0].count, 20);
    }

    #[test]
    fn the_cause_drops_frames_that_restate_the_scope_and_is_cut() {
        let e = err(&format!("{} the root cause", "x".repeat(400)))
            .context("evaluating BUILD")
            .context("resolving target `//a:b`");
        let line = cause_line(&e, "//a:b");
        assert!(line.ends_with("the root cause"), "{line}");
        assert_eq!(line.chars().count(), CAUSE_MAX_CHARS + 1);
        assert!(line.starts_with('…'));
        assert!(!line.contains("//a:b"), "{line}");

        let multi = err("first line\nsecond line");
        assert_eq!(cause_line(&multi, "//a:b"), "first line");
    }

    /// Only the frame naming the scope itself is dropped: a dependency whose
    /// `get_def` wraps with an unquoted addr ahead of the provider's frame,
    /// and the error comes back through a memoizer cell flattened to one frame.
    #[test]
    fn the_cause_skips_every_frame_naming_the_scope() {
        let e = err("go list: exit status 1")
            .context("resolving target `//p:b`")
            .context("get_def: //p:b");
        // A second waiter on the cell keeps the `Arc` shared, which is when
        // the memoizer hands back its flattening wrapper.
        let arc = Arc::new(e);
        let _other_waiter = Arc::clone(&arc);
        let shared = hcore::hmemoizer::unwrap_arc_err(arc);
        assert_eq!(cause_line(&shared, "//p:b"), "go list: exit status 1");
    }

    /// A package scope keeps a frame naming a path under it, and the root
    /// package keeps a frame naming any package.
    #[test]
    fn the_cause_keeps_frames_naming_something_under_the_scope() {
        let e = err("Parse error")
            .context("evaluating `//a/b/BUILD`")
            .context("probing package `//a`");
        assert_eq!(
            cause_line(&e, "//a"),
            "evaluating `//a/b/BUILD`: Parse error"
        );
        let root = err("Parse error")
            .context("evaluating `//a/BUILD`")
            .context("probing package `//`");
        assert_eq!(
            cause_line(&root, "//"),
            "evaluating `//a/BUILD`: Parse error"
        );
    }

    /// The final frame is the root cause even when it names the scope.
    #[test]
    fn the_cause_never_drops_the_last_frame() {
        let e = err("target //p:b not found");
        assert_eq!(cause_line(&e, "//p:b"), "target //p:b not found");
    }

    /// Only the frame naming the scope itself is dropped: a dependency whose
    /// name starts with the scope's is the cause, not a restatement.
    #[test]
    fn the_cause_keeps_a_frame_naming_a_longer_addr() {
        let e = err("exit status 1")
            .context("building `//p:ab`")
            .context("resolving target `//p:a`");
        assert_eq!(cause_line(&e, "//p:a"), "building `//p:ab`: exit status 1");
    }

    #[test]
    fn the_cause_carries_no_control_bytes() {
        let e = err("\u{1b}[31mred\u{1b}[0m failure");
        assert_eq!(cause_line(&e, "//p:a"), "[31mred[0m failure");
    }

    #[test]
    fn fail_fast_turns_keep_going_off_at_the_call_site() {
        let gaps = Gaps::new("//...");
        assert!(matches!(
            Discovery::keep_going_unless(true, &gaps),
            Discovery::Complete
        ));
        assert!(matches!(
            Discovery::keep_going_unless(false, &gaps),
            Discovery::KeepGoing(_)
        ));
    }

    #[test]
    fn the_provider_rides_through_context() {
        let e = err("go list: exit status 1").context(InProvider {
            provider: "go".into(),
            what: "resolving target `//a:b`".into(),
        });
        assert_eq!(provider_of(&e), "go");
        assert_eq!(e.to_string(), "resolving target `//a:b`");
        assert_eq!(provider_of(&err("bare")), "");
    }
}
