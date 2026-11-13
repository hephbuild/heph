//! The origin ids the Go drivers give their `deps` inputs, and the lookup `run`
//! does with them.
//!
//! `parse` names the `i`-th dep of group `group` [`dep_origin`]`(group, i)`, and
//! records how many deps each group declared in a [`DepGroups`] on the def. `run`
//! rebuilds the same origins from that record and finds each input by equality,
//! rather than decoding origin ids by prefix.

use hdriver_support::driver_managed::ManagedRunRequest;
use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;

/// Origin id of the `i`-th dep of group `group`. The one place it is spelled:
/// `parse` names the input with it and `run` looks the input up by it.
pub(crate) fn dep_origin(group: &str, i: usize) -> String {
    format!("dep|{group}|{i}")
}

/// How many deps each group of a target's `deps` declared, kept on the def so
/// `run` can rebuild every origin `parse` handed out.
///
/// Never hashed: the deps are inputs, whose order and content the engine
/// already folds into the cache key, so a count derived from them adds nothing.
/// `#[serde(default)]` at each use, so a def serialized before the field existed
/// still decodes (to "no deps", which `run` reads as nothing staged).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct DepGroups(BTreeMap<String, usize>);

impl DepGroups {
    pub(crate) fn of(deps: &HashMap<String, Vec<String>>) -> Self {
        Self(deps.iter().map(|(g, a)| (g.clone(), a.len())).collect())
    }

    /// The origins of `group`'s deps, in declaration order.
    fn origins<'s>(&'s self, group: &'s str) -> impl Iterator<Item = String> + 's {
        let n = self.0.get(group).copied().unwrap_or(0);
        (0..n).map(move |i| dep_origin(group, i))
    }

    /// Every file `group`'s deps staged, in declaration order.
    ///
    /// Per origin, not per input: a dep that produces several artifacts (an fs
    /// glob, a `group`) reaches the driver as several inputs sharing one origin,
    /// and that origin's one list file already names every file. Reading the list
    /// once per input yields each file once per artifact.
    ///
    /// An origin with no staged input contributes nothing: the engine elides a
    /// dep that produced no artifacts (a query that matched nothing), and a
    /// hash-only (`runtime: false`) dep is never staged.
    pub(crate) fn staged_paths(
        &self,
        req: &ManagedRunRequest<'_, '_>,
        group: &str,
    ) -> anyhow::Result<Vec<String>> {
        use anyhow::Context as _;
        let mut out: Vec<String> = Vec::new();
        for origin in self.origins(group) {
            // A support input can share its dep's origin but has no list file.
            let Some(list_path) = req
                .inputs
                .iter()
                .filter(|m| m.input.origin_id == origin)
                .find_map(|m| m.list_path.as_deref())
            else {
                continue;
            };
            let f = std::fs::File::open(list_path)
                .with_context(|| format!("open dep list {list_path:?} of {origin}"))?;
            for line in std::io::BufReader::new(f).lines() {
                let line = line.with_context(|| format!("read dep list {list_path:?}"))?;
                if !line.is_empty() {
                    out.push(line);
                }
            }
        }
        Ok(out)
    }
}
