use std::sync::Arc;

/// The longest driver name a listing may carry. A longer one is stored as
/// unknown: no real driver name is anywhere near it, and a value that large in
/// every entry of every package is not "cheap or absent".
pub const MAX_LISTED_DRIVER_LEN: usize = 256;

/// What `get` (labels, driver) and the def's `parse` (`has_codegen`) give an
/// addr, **if it resolves**, under the `states` the `list` call received.
///
/// Every field is `None` (unknown) or `Some` (exact). There are no bounds: a
/// provider that cannot name a field exactly leaves it unknown, and that entry
/// takes the resolve-the-spec path.
///
/// A listing is a candidate set, so the facts are conditional on existence: a
/// provider may put facts on an entry `get` later declines. The engine trusts a
/// listed **No** (it drops the candidate without `get`) and treats a listed
/// **Yes** as a candidate whose existence is confirmed exactly as for any other.
///
/// Normalized at construction — labels sorted and deduplicated, an empty or
/// oversized driver stored as unknown — so equality is set equality and no
/// illegal value exists.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedFacts {
    labels: Option<Arc<[String]>>,
    driver: Option<Arc<str>>,
    has_codegen: Option<bool>,
}

impl ListedFacts {
    /// Every field unknown.
    pub const UNKNOWN: Self = Self {
        labels: None,
        driver: None,
        has_codegen: None,
    };

    /// The labels `get` returns for this addr, exactly. An empty set is "no
    /// labels", never unknown.
    pub fn with_labels(self, labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut labels: Vec<String> = labels.into_iter().map(Into::into).collect();
        labels.sort_unstable();
        labels.dedup();
        self.with_label_set(labels.into())
    }

    /// [`with_labels`](Self::with_labels) for a set the caller already shares
    /// across entries. Normalized only when it is not sorted and deduplicated
    /// already, so a shared set stays shared.
    pub fn with_shared_labels(self, labels: Arc<[String]>) -> Self {
        if labels.iter().zip(labels.iter().skip(1)).all(|(a, b)| a < b) {
            self.with_label_set(labels)
        } else {
            self.with_labels(labels.iter().cloned())
        }
    }

    fn with_label_set(mut self, labels: Arc<[String]>) -> Self {
        self.labels = Some(labels);
        self
    }

    /// The driver `get` returns for this addr, exactly. An empty or oversized
    /// name is stored as unknown.
    pub fn with_driver(mut self, driver: impl Into<Arc<str>>) -> Self {
        let driver = driver.into();
        self.driver =
            (!driver.is_empty() && driver.len() <= MAX_LISTED_DRIVER_LEN).then_some(driver);
        self
    }

    /// Whether any path in the def's outputs has a codegen mode other than
    /// none. `support_files` don't count.
    pub fn with_has_codegen(mut self, has_codegen: bool) -> Self {
        self.has_codegen = Some(has_codegen);
        self
    }

    /// Sorted and deduplicated, when known.
    pub fn labels(&self) -> Option<&[String]> {
        self.labels.as_deref()
    }

    /// The shared label set, when known.
    pub fn label_set(&self) -> Option<&Arc<[String]>> {
        self.labels.as_ref()
    }

    pub fn driver(&self) -> Option<&str> {
        self.driver.as_deref()
    }

    /// The shared driver name, when known.
    pub fn driver_arc(&self) -> Option<&Arc<str>> {
        self.driver.as_ref()
    }

    pub fn has_codegen(&self) -> Option<bool> {
        self.has_codegen
    }

    /// Whether any field is known.
    pub fn is_unknown(&self) -> bool {
        self.labels.is_none() && self.driver.is_none() && self.has_codegen.is_none()
    }

    /// Two listings of one target reconciled: per field, the value when both
    /// know it and agree, unknown otherwise. Commutative and associative, so a
    /// fold over any number of listings is independent of their order.
    pub fn agree(&mut self, other: &Self) {
        if self.labels != other.labels {
            self.labels = None;
        }
        if self.driver != other.driver {
            self.driver = None;
        }
        if self.has_codegen != other.has_codegen {
            self.has_codegen = None;
        }
    }

    /// Every field unknown.
    pub fn forget(&mut self) {
        *self = Self::UNKNOWN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_normalized_and_compared_as_a_set() {
        let a = ListedFacts::default().with_labels(["b", "a", "b"]);
        let b = ListedFacts::default().with_labels(["a", "b"]);
        assert_eq!(a, b);
        assert_eq!(a.labels(), Some(&["a".to_string(), "b".to_string()][..]));
        let shared: Arc<[String]> = vec!["z".to_string(), "a".to_string()].into();
        assert_eq!(
            ListedFacts::default().with_shared_labels(shared).labels(),
            Some(&["a".to_string(), "z".to_string()][..])
        );
    }

    #[test]
    fn empty_label_set_is_known() {
        let f = ListedFacts::default().with_labels(Vec::<String>::new());
        assert_eq!(f.labels(), Some(&[][..]));
        let one_empty = ListedFacts::default().with_labels([""]);
        assert_eq!(one_empty.labels(), Some(&[String::new()][..]));
    }

    #[test]
    fn empty_or_oversized_driver_is_unknown() {
        assert_eq!(ListedFacts::default().with_driver("").driver(), None);
        let long = "x".repeat(MAX_LISTED_DRIVER_LEN + 1);
        assert_eq!(ListedFacts::default().with_driver(long).driver(), None);
        let max = "x".repeat(MAX_LISTED_DRIVER_LEN);
        assert_eq!(
            ListedFacts::default().with_driver(max.as_str()).driver(),
            Some(max.as_str())
        );
    }

    /// C19: `has_codegen` merges like the other fields.
    #[test]
    fn has_codegen_merge() {
        let f = |c: Option<bool>| match c {
            Some(c) => ListedFacts::default().with_has_codegen(c),
            None => ListedFacts::default(),
        };
        for (a, b, want) in [
            (Some(false), Some(false), Some(false)),
            (Some(true), Some(true), Some(true)),
            (Some(false), Some(true), None),
            (Some(false), None, None),
            (None, None, None),
        ] {
            let mut m = f(a);
            m.agree(&f(b));
            assert_eq!(m.has_codegen(), want, "{a:?} + {b:?}");
        }
    }
}
