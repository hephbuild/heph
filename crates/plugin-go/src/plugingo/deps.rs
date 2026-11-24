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

/// How `parse` declares one dep: the annotations it carries and whether it is
/// staged. Every dep is hashed.
pub(crate) struct DepShape {
    pub(crate) annotations: BTreeMap<String, String>,
    /// `false` for a hash-only dep: it keys the target but is never staged.
    pub(crate) runtime: bool,
}

impl DepShape {
    /// A staged dep with each of `flags` (a `stage` annotation) set to `"true"`.
    pub(crate) fn staged(flags: &[&str]) -> Self {
        Self {
            annotations: flags
                .iter()
                .map(|f| (f.to_string(), "true".to_string()))
                .collect(),
            runtime: true,
        }
    }
}

/// The input for the `i`-th dep of `group`, origin [`dep_origin`]`(group, i)`.
pub(crate) fn dep_input(
    r#ref: hplugin::driver::TargetAddr,
    group: &str,
    i: usize,
    shape: DepShape,
) -> hplugin::driver::targetdef::Input {
    hplugin::driver::targetdef::Input {
        r#ref,
        mode: hplugin::driver::targetdef::InputMode::Standard,
        origin_id: dep_origin(group, i),
        annotations: shape.annotations,
        hashed: true,
        runtime: shape.runtime,
    }
}

/// The inputs for a target's `deps`, resolved against `pkg`: groups in sorted
/// order, so `HashMap` iteration order cannot reach the input list (or a def
/// hash that folds it in), and each group's deps in declared order. `shape`
/// gives the `i`-th dep of a group its [`DepShape`].
pub(crate) fn dep_inputs(
    deps: &HashMap<String, Vec<String>>,
    pkg: &hmodel::htpkg::PkgBuf,
    shape: impl Fn(&str, usize) -> DepShape,
) -> anyhow::Result<Vec<hplugin::driver::targetdef::Input>> {
    use anyhow::Context as _;
    let mut groups: Vec<(&String, &Vec<String>)> = deps.iter().collect();
    groups.sort_unstable_by_key(|(group, _)| *group);
    let mut out = Vec::with_capacity(groups.iter().map(|(_, addrs)| addrs.len()).sum());
    for (group, addrs) in groups {
        for (i, addr) in addrs.iter().enumerate() {
            let r#ref = hplugin::driver::TargetAddr::parse(addr, pkg)
                .with_context(|| format!("parse dep addr {addr}"))?;
            out.push(dep_input(r#ref, group, i, shape(group, i)));
        }
    }
    Ok(out)
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
    pub(crate) fn origins<'s>(&'s self, group: &'s str) -> impl Iterator<Item = String> + 's {
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

    /// The first file `group`'s deps staged: the one path of a one-output
    /// group (a lib archive, its facts, a tool binary). Errors when the group
    /// staged nothing.
    pub(crate) fn single_staged_path(
        &self,
        req: &ManagedRunRequest<'_, '_>,
        group: &str,
    ) -> anyhow::Result<String> {
        self.staged_paths(req, group)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no staged file for dep group {group}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdriver_support::driver_managed::ManagedRunInput;
    use hplugin::driver::inputartifact::{InputArtifact, Type};
    use hplugin::driver::targetdef::TargetDef;

    /// A staged input of origin `origin`, its list file at `list` (`None` for a
    /// support input, which has none).
    fn input(origin: &str, list: Option<std::path::PathBuf>) -> ManagedRunInput {
        ManagedRunInput {
            input: hplugin::driver::RunInput {
                artifact: InputArtifact {
                    r#type: Type::Dep,
                    origin_id: origin.to_string(),
                    // Never read: the lookup goes through the list file.
                    content: std::sync::Arc::new(hcore::hartifactcontent::FileContent::new(
                        "unused",
                    )),
                },
                origin_id: origin.to_string(),
                source_addr: hmodel::htaddr::parse_addr("//dep:x").expect("addr"),
                filters: vec![],
                annotations: Default::default(),
            },
            list_path: list,
            unpack_root: std::path::PathBuf::new(),
        }
    }

    fn list_file(dir: &std::path::Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n")).expect("write list");
        p
    }

    /// `single_staged_path` over `inputs`, for a def whose `lib` group
    /// declared `n` deps.
    fn single(inputs: Vec<ManagedRunInput>, n: usize) -> anyhow::Result<String> {
        let def = TargetDef::default();
        let id = "t".to_string();
        let req = ManagedRunRequest {
            request: hplugin::driver::RunRequest::new(&id, &def, std::path::PathBuf::new()),
            sandbox_dir: std::path::PathBuf::new(),
            sandbox_ws_dir: std::path::PathBuf::new(),
            sandbox_pkg_dir: std::path::PathBuf::new(),
            inputs,
        };
        DepGroups(BTreeMap::from([("lib".to_string(), n)])).single_staged_path(&req, "lib")
    }

    /// Several inputs sharing one origin (a dep that produced several
    /// artifacts, or a support input beside it): the first list file found is
    /// the origin's, and a support input without one is passed over.
    #[test]
    fn shared_origin_reads_the_list_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let list = list_file(dir.path(), "a.list", &["lib/a.a", "lib/b.a"]);
        let origin = dep_origin("lib", 0);
        let got = single(
            vec![
                input(&origin, None),
                input(&origin, Some(list.clone())),
                input(&origin, Some(list)),
            ],
            1,
        )
        .expect("staged");
        assert_eq!(got, "lib/a.a");
    }

    /// The group's first dep staged nothing (elided by the engine, or its list
    /// is empty): the path comes from the next one.
    #[test]
    fn empty_first_origin_falls_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = list_file(dir.path(), "empty.list", &[]);
        let second = list_file(dir.path(), "second.list", &["lib/second.a"]);
        let got = single(
            vec![
                input(&dep_origin("lib", 0), Some(empty)),
                input(&dep_origin("lib", 2), Some(second)),
            ],
            3,
        )
        .expect("staged");
        assert_eq!(got, "lib/second.a");
    }

    /// No input carries the group's origin (or the group declared no deps):
    /// the error names the group.
    #[test]
    fn absent_origin_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let other = list_file(dir.path(), "other.list", &["other/x.a"]);
        for n in [0, 1] {
            let err = single(vec![input(&dep_origin("other", 0), Some(other.clone()))], n)
                .expect_err("nothing staged for lib");
            assert_eq!(err.to_string(), "no staged file for dep group lib");
        }
    }
}
