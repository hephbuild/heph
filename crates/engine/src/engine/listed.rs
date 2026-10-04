//! What each provider's `list` said about a target's labels, held per request
//! so the spec it later resolves to can be checked against it.
//!
//! A label selector decides membership from these listings without resolving
//! the candidates. That makes a listing a promise: a provider that lists
//! labels `get` does not return would silently select the wrong targets.
//!
//! The check runs on every *read* of a listed addr's spec
//! (`Engine::get_spec_no_track`), not once inside the memoized resolve: a spec
//! resolved before its listing was recorded — as the dependency of a target
//! already building — is still compared the next time anything reads it,
//! which includes the walk confirming a listed match. So a listed "yes" that
//! is wrong always fails the run, whatever the scheduling.
//!
//! Accepted exemption: a listed "no" that is wrong is never resolved by the
//! selection, so it is caught only if something else reads that spec in the
//! same request. That is the whole point of the listing — not resolving the
//! rejects — and `go_list_labels_equal_spec_labels` holds the go provider to
//! its table instead.
//!
//! Only what the walk actually matched against is recorded: when providers
//! list the same addr with different sets, or one of them does not know, the
//! walk treats its labels as unknown and resolves it (see `Engine::select`),
//! and nothing is recorded for it.

use hmodel::htaddr::Addr;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One provider's listing of one addr.
#[derive(Debug, Clone)]
struct Listing {
    /// Index into the engine's provider list — the name is only needed to
    /// report a mismatch.
    provider: usize,
    /// Sorted and deduplicated, so the check compares sets.
    labels: Arc<[String]>,
}

#[derive(Debug, Default)]
struct Inner {
    by_addr: FxHashMap<Addr, Listing>,
    /// Every distinct label set seen this request. A Go workspace lists ~10
    /// targets per package over a handful of distinct sets; one copy each.
    interned: FxHashSet<Arc<[String]>>,
}

/// Per-request record of listed labels. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct ListedLabels {
    /// Set once anything is recorded. Every spec read checks it first, so a
    /// request that never selected by label — `//...`, a single addr — pays an
    /// atomic load per read and never the lock.
    any: AtomicBool,
    inner: Mutex<Inner>,
}

impl ListedLabels {
    /// Record that `provider` listed `addr` with `labels`, and return the
    /// normalized set the walk should match against.
    ///
    /// The walk merges every provider's listing of an addr before calling
    /// this, so one addr is recorded with one set. Should another walk on the
    /// same request record it again, the first set stands — the same package
    /// lists the same way within a request.
    pub(crate) fn record(&self, addr: &Addr, provider: usize, labels: &[String]) -> Arc<[String]> {
        // Before the entry, so any read ordered after this call returns sees
        // the flag set and takes the lock that publishes the entry. A read
        // racing the call may miss both, and needs neither: nothing selected
        // on this listing yet.
        self.any.store(true, Ordering::Release);
        let mut inner = self.inner.lock();
        if let Some(listing) = inner.by_addr.get(addr) {
            return Arc::clone(&listing.labels);
        }
        let labels = intern(&mut inner.interned, labels);
        inner.by_addr.insert(
            addr.clone(),
            Listing {
                provider,
                labels: Arc::clone(&labels),
            },
        );
        labels
    }

    /// The listing `spec_labels` must agree with, if `addr` was listed with
    /// labels and they differ: `(listing provider index, listed set)`.
    pub(crate) fn contradiction(
        &self,
        addr: &Addr,
        spec_labels: &[String],
    ) -> Option<(usize, Arc<[String]>)> {
        if !self.any.load(Ordering::Acquire) {
            return None;
        }
        let inner = self.inner.lock();
        let listing = inner.by_addr.get(addr)?;
        let resolved = normalized(spec_labels);
        (*listing.labels != *resolved).then(|| (listing.provider, Arc::clone(&listing.labels)))
    }
}

fn normalized(labels: &[String]) -> std::borrow::Cow<'_, [String]> {
    if labels.is_sorted_by(|a, b| a < b) {
        return std::borrow::Cow::Borrowed(labels);
    }
    let mut owned = labels.to_vec();
    owned.sort_unstable();
    owned.dedup();
    std::borrow::Cow::Owned(owned)
}

fn intern(set: &mut FxHashSet<Arc<[String]>>, labels: &[String]) -> Arc<[String]> {
    let labels = normalized(labels);
    if let Some(shared) = set.get(&*labels) {
        return Arc::clone(shared);
    }
    let shared: Arc<[String]> = labels.into_owned().into();
    set.insert(Arc::clone(&shared));
    shared
}

/// A provider listed a target with labels its resolved spec does not carry.
///
/// Fatal rather than a warning: a label selection already acted on the
/// listing, so the selected set is wrong in a way no later step corrects.
#[derive(Debug, Clone)]
pub struct ListedLabelsMismatch {
    pub addr: Addr,
    /// The provider whose `list` made the claim.
    pub listed_by: String,
    /// The provider whose `get` produced the spec, when it is a different one.
    pub resolved_by: Option<String>,
    pub listed: Vec<String>,
    pub resolved: Vec<String>,
}

impl fmt::Display for ListedLabelsMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let addr = self.addr.format();
        write!(
            f,
            "label selection is unreliable for {addr}: provider `{}` listed it with labels [{}], but its spec",
            self.listed_by,
            self.listed.join(", "),
        )?;
        if let Some(other) = &self.resolved_by {
            write!(f, " (from provider `{other}`)")?;
        }
        write!(f, " has [{}]", self.resolved.join(", "))?;
        let only_listed: Vec<&str> = difference(&self.listed, &self.resolved);
        let only_resolved: Vec<&str> = difference(&self.resolved, &self.listed);
        if !only_listed.is_empty() {
            write!(
                f,
                "; listed but not in the spec: {}",
                only_listed.join(", ")
            )?;
        }
        if !only_resolved.is_empty() {
            write!(
                f,
                "; in the spec but not listed: {}",
                only_resolved.join(", ")
            )?;
        }
        write!(
            f,
            ". This is a bug in provider `{}`, not in your BUILD files: its `list` must report exactly the labels its `get` returns. Selecting the target by address (`heph run {addr}`) does not use the listing",
            self.listed_by,
        )
    }
}

/// Labels in `a` and not in `b`. Both are short and sorted.
fn difference<'a>(a: &'a [String], b: &[String]) -> Vec<&'a str> {
    a.iter()
        .filter(|l| !b.contains(l))
        .map(String::as_str)
        .collect()
}

impl std::error::Error for ListedLabelsMismatch {}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|l| (*l).to_string()).collect()
    }

    #[test]
    fn listed_labels_compare_as_sets_and_intern() {
        let listed = ListedLabels::default();
        let a = Addr::new(PkgBuf::from("p"), "a".into(), Default::default());
        let b = Addr::new(PkgBuf::from("p"), "b".into(), Default::default());

        let la = listed.record(&a, 0, &s(&["y", "x", "x"]));
        let lb = listed.record(&b, 0, &s(&["x", "y"]));
        assert_eq!(&*la, &s(&["x", "y"])[..]);
        assert!(Arc::ptr_eq(&la, &lb), "one copy per distinct set");

        assert!(listed.contradiction(&a, &s(&["y", "x"])).is_none());
        assert!(listed.contradiction(&a, &s(&["x"])).is_some());
        assert!(listed.contradiction(&a, &s(&["x", "y", "z"])).is_some());
        // An unlisted addr is never contradicted.
        let c = Addr::new(PkgBuf::from("p"), "c".into(), Default::default());
        assert!(listed.contradiction(&c, &s(&["anything"])).is_none());
        // The first listing wins.
        listed.record(&a, 1, &s(&["z"]));
        assert!(listed.contradiction(&a, &s(&["x", "y"])).is_none());
    }
}
