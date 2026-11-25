//! The loaded files' interfaces, memoized across typechecks (I10).
//!
//! Each file a document's loads reach is typechecked once and kept under a
//! **chained key**: the digest of its content plus, for each of its loads,
//! the load string and the keys of the files it resolved to. A file's key so
//! moves whenever anything it sees moves, however deep, and an entry whose
//! key still matches was checked against exactly the entries its loads see
//! now. The content digest is validated with a stat and recomputed only when
//! the stat changes.
//!
//! The load walk is memoized too, one per document, under its package, load
//! strings and BUILD patterns. It is validated by a stat of every path it
//! read ([`WalkReads`]): each file it parsed, each path a load probed (so a
//! file created where a load found nothing is seen), and each directory a
//! load listed (so a BUILD file added to a loaded directory is seen). It runs
//! again, through evaluation's resolver, only when one of those moved.
//!
//! A stat is **racily clean** when the file changed within [`RACY`] of the
//! moment it was read: a same-length rewrite inside one timestamp tick would
//! keep the stat, and so the old answer. Such a stat validates nothing, and
//! the file is read again on the next lookup, until it is safely old.
//!
//! An entry holds the `TypeInstanceId`s its typecheck minted, across
//! requests, on purpose: a document typechecked twice against the same
//! entries sees the same record types. I4 governs lint's checks, not this
//! one; an id never reaches any output.

use crate::pluginbuildfile::checks::lint::{Checked, check_loaded, interface_with};
use crate::pluginbuildfile::checks::loads::{LoadEdge, LoadWalk, WalkedFile, walk_loads};
use starlark::environment::Globals;
use starlark::typing::Interface;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use xxhash_rust::xxh3::{Xxh3Default, xxh3_128};

/// How long after a change a stat stays untrusted: the coarsest timestamp
/// tick a workspace's filesystem may have (FAT's is 2s; ext4's and APFS's
/// are far finer), so a rewrite within one tick of a read cannot hide.
const RACY: Duration = Duration::from_secs(2);

/// One inode's identity, size and times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Meta {
    dev: u64,
    ino: u64,
    mode: u32,
    len: u64,
    /// Nanoseconds since the epoch.
    mtime: i128,
    ctime: i128,
}

impl Meta {
    fn of(meta: &std::fs::Metadata) -> Meta {
        use std::os::unix::fs::MetadataExt;
        let ns = |s: i64, n: i64| i128::from(s) * 1_000_000_000 + i128::from(n);
        Meta {
            dev: meta.dev(),
            ino: meta.ino(),
            mode: meta.mode(),
            len: meta.len(),
            mtime: ns(meta.mtime(), meta.mtime_nsec()),
            ctime: ns(meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// The last time anything about the inode changed.
    fn changed(&self) -> i128 {
        self.mtime.max(self.ctime)
    }
}

/// What a stat says of a path: if any of it moves, the path is read again.
/// The link itself, not followed, so a file replaced by a link moves it; and
/// for a link, what it points at (`None` when that is missing).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    link: Meta,
    target: Option<Meta>,
}

impl Stat {
    /// `None` when nothing is at `path` (or it cannot be stat'ed).
    fn of(path: &Path) -> Option<Stat> {
        let link = std::fs::symlink_metadata(path).ok()?;
        let target = if link.file_type().is_symlink() {
            std::fs::metadata(path).ok().map(|m| Meta::of(&m))
        } else {
            None
        };
        Some(Stat {
            link: Meta::of(&link),
            target,
        })
    }

    /// Whether the path changed within [`RACY`] of `read_at` (nanoseconds
    /// since the epoch), or after it: a further change could then keep this
    /// very stat.
    fn racy(&self, read_at: i128) -> bool {
        let changed = self.target.map_or(self.link.changed(), |t| {
            t.changed().max(self.link.changed())
        });
        changed + RACY.as_nanos() as i128 > read_at
    }
}

/// Nanoseconds since the epoch, now.
fn now() -> i128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128)
}

/// One loaded file's memo entry.
struct Entry {
    stat: Option<Stat>,
    /// The stat was racily clean when the content was read, so it does not
    /// vouch for the digest.
    racy: bool,
    /// The content's digest; [`UNREADABLE`] when it could not be read.
    digest: u128,
    /// The chained key the entry was checked under.
    key: u128,
    /// `None`: the file does not parse, so a load of it is left out.
    checked: Option<Checked>,
}

/// The digest of a file that could not be read (or is not UTF-8).
const UNREADABLE: u128 = 0;

/// One document's memoized load walk.
struct Walk {
    pkg: String,
    modules: Vec<String>,
    patterns: Vec<glob::Pattern>,
    edges: BTreeMap<String, LoadEdge>,
    walk: LoadWalk,
    /// Every path the walk read, and its stat once it had: the walk holds
    /// while each still stats the same.
    stamps: HashMap<PathBuf, Option<Stat>>,
    /// A stamp was racily clean: the walk runs again on the next lookup.
    racy: bool,
}

impl Walk {
    /// Walk the loads afresh; `read_at` is a time before it started.
    fn run(
        root: &Path,
        pkg: &str,
        file: &Path,
        modules: &[&str],
        patterns: &[glob::Pattern],
        read_at: i128,
    ) -> (Walk, u64) {
        let walker = hwalk::CachedWalker::disabled();
        let (edges, walk, reads) =
            walk_loads(root, &walker, pkg, file, modules.iter().copied(), patterns);
        // Stamped after the walk: a path that changed while it ran changed
        // after `read_at`, so it is racy and the next lookup walks again.
        let mut stamps = HashMap::with_capacity(reads.paths.len());
        let mut racy = false;
        for path in reads.paths {
            stamps.entry(path).or_insert_with_key(|path| {
                let stat = Stat::of(path);
                racy |= stat.as_ref().is_some_and(|s| s.racy(read_at));
                stat
            });
        }
        let walked = Walk {
            pkg: pkg.to_owned(),
            modules: modules.iter().map(|&m| m.to_owned()).collect(),
            patterns: patterns.to_vec(),
            edges,
            walk,
            stamps,
            racy,
        };
        (walked, reads.parsed)
    }

    /// Whether this walk is still the answer for these inputs: they are the
    /// ones it walked, and nothing it read has moved.
    fn holds(&self, pkg: &str, modules: &[&str], patterns: &[glob::Pattern]) -> bool {
        !self.racy
            && self.pkg == pkg
            && self
                .modules
                .iter()
                .map(String::as_str)
                .eq(modules.iter().copied())
            && self.patterns == patterns
            && self
                .stamps
                .iter()
                .all(|(path, stamp)| Stat::of(path) == *stamp)
    }
}

/// What one document's loads see.
pub(crate) struct Loaded {
    /// Each load string that resolved and typechecked, and its interface.
    pub(crate) interfaces: HashMap<String, Interface>,
    /// The chained keys of the document's loads, combined: equal across two
    /// lookups exactly when every interface is.
    pub(crate) key: u128,
    /// Every file the loads reach, transitively: the entries this document
    /// keeps alive.
    pub(crate) closure: Vec<String>,
}

/// The memo of loaded files' interfaces, one per server.
#[derive(Default)]
pub(crate) struct InterfaceMemo {
    /// The registry generation the entries were checked against: the globals
    /// change with it, so a new one starts the memo over.
    generation: u64,
    /// By workspace-relative path.
    entries: HashMap<String, Entry>,
    /// Each open document's load walk, by its absolute path. It does not
    /// depend on the globals, so it survives a new generation.
    walks: HashMap<PathBuf, Walk>,
    /// Loaded-file typechecks run, for `heph/documents` (test support).
    checks: u64,
    /// Load walks run, for `heph/documents` (test support).
    walks_run: u64,
    /// Loaded files read from disk, by a walk or for a digest, for
    /// `heph/documents` (test support).
    reads: u64,
}

impl InterfaceMemo {
    /// The interface each of a document's `load()` strings sees: lint's walk
    /// and lint's merge (I9), the loaded files read from disk under `root`.
    ///
    /// `file` is the document's absolute path (`root` joined with its
    /// workspace path), `pkg` its package, `modules` its load strings,
    /// `globals` lint's globals at registry generation `generation`. A load
    /// that does not resolve or parse is missing from the map, so the names
    /// it binds are untyped.
    #[expect(clippy::too_many_arguments, reason = "one lookup's inputs")]
    pub(crate) fn loaded_interfaces<'m>(
        &mut self,
        root: &Path,
        pkg: &str,
        file: &Path,
        modules: impl IntoIterator<Item = &'m str>,
        patterns: &[glob::Pattern],
        globals: &Globals,
        generation: u64,
    ) -> Loaded {
        if generation != self.generation {
            self.entries.clear();
            self.generation = generation;
        }
        // Before anything is read: every read below is after it.
        let read_at = now();
        let modules: Vec<&str> = modules.into_iter().collect();
        let (file, walk) = match self.walks.remove_entry(file) {
            Some((file, walk)) if walk.holds(pkg, &modules, patterns) => (file, walk),
            prev => {
                let (walk, parsed) = Walk::run(root, pkg, file, &modules, patterns, read_at);
                self.walks_run += 1;
                self.reads += parsed;
                (
                    prev.map_or_else(|| file.to_path_buf(), |(file, _)| file),
                    walk,
                )
            }
        };
        let loaded = self.lookup(root, &walk, globals, read_at);
        self.walks.insert(file, walk);
        loaded
    }

    /// The interfaces `walk` gives its document, each entry it reaches
    /// brought up to date first.
    fn lookup(&mut self, root: &Path, walk: &Walk, globals: &Globals, read_at: i128) -> Loaded {
        let Walk {
            edges,
            walk,
            stamps,
            ..
        } = walk;
        let closure: Vec<String> = walk.files.keys().cloned().collect();
        let Ok(order) = walk.dependency_order() else {
            // Nothing typechecks, whatever the files hold: the key is the
            // walk's shape.
            let mut h = Xxh3Default::new();
            h.update(b"unordered");
            for (module, edge) in edges {
                chain_edge(&mut h, module, edge, |_| UNREADABLE);
            }
            return Loaded {
                interfaces: HashMap::new(),
                key: h.digest128(),
                closure,
            };
        };
        let mut keys: HashMap<&str, u128> = HashMap::with_capacity(order.len());
        for path in order {
            let Some(walked) = walk.files.get(path) else {
                continue;
            };
            let abs = root.join(path);
            // The walk stat'ed every file it parsed, at this lookup or when
            // it ran; a file it did not is stat'ed now.
            let stat = match stamps.get(&abs) {
                Some(stat) => stat.clone(),
                None => Stat::of(&abs),
            };
            let key = self.refresh(path, &abs, stat, walked, &keys, globals, read_at);
            keys.insert(path, key);
        }

        let entries = &self.entries;
        let checked = |path: &str| entries.get(path).and_then(|e| e.checked.as_ref());
        let interfaces = edges
            .iter()
            .filter_map(|(module, edge)| {
                Some((module.clone(), interface_with(edge, checked).ok()?))
            })
            .collect();
        let mut h = Xxh3Default::new();
        for (module, edge) in edges {
            chain_edge(&mut h, module, edge, |p| {
                keys.get(p).copied().unwrap_or(UNREADABLE)
            });
        }
        Loaded {
            interfaces,
            key: h.digest128(),
            closure,
        }
    }

    /// Bring `path`'s entry up to date and return its chained key: rehash it
    /// if its stat moved or was racy, and typecheck it again if its key did.
    /// `stat` is the file's stat now, `keys` holds the keys of the files it
    /// loads, `read_at` is a time before this lookup read anything.
    #[expect(clippy::too_many_arguments, reason = "one entry's inputs")]
    fn refresh(
        &mut self,
        path: &str,
        abs: &Path,
        stat: Option<Stat>,
        walked: &WalkedFile,
        keys: &HashMap<&str, u128>,
        globals: &Globals,
        read_at: i128,
    ) -> u128 {
        let prev = self.entries.remove(path);
        let mut src = None;
        let (digest, racy) = match &prev {
            Some(e) if !e.racy && stat.is_some() && e.stat == stat => (e.digest, false),
            _ => {
                src = read(abs);
                self.reads += 1;
                let digest = src
                    .as_deref()
                    .map_or(UNREADABLE, |s| xxh3_128(s.as_bytes()));
                (digest, stat.as_ref().is_some_and(|s| s.racy(read_at)))
            }
        };
        let key = chain(digest, walked, keys);
        if let Some(mut e) = prev
            && e.key == key
        {
            e.stat = stat;
            e.racy = racy;
            e.digest = digest;
            self.entries.insert(path.to_owned(), e);
            return key;
        }
        // The stat held but a load's key moved: the content is the digest's.
        let src = src.or_else(|| {
            self.reads += 1;
            read(abs)
        });
        let entries = &self.entries;
        let checked = src.and_then(|src| {
            check_loaded(
                path,
                src,
                walked,
                |p| entries.get(p).and_then(|e| e.checked.as_ref()),
                globals,
            )
        });
        self.checks += 1;
        self.entries.insert(
            path.to_owned(),
            Entry {
                stat,
                racy,
                digest,
                key,
                checked,
            },
        );
        key
    }

    /// Drop every entry no open document's load closure reaches.
    pub(crate) fn retain(&mut self, reached: &HashSet<&str>) {
        self.entries
            .retain(|path, _| reached.contains(path.as_str()));
    }

    /// Drop the load walk of the document at `file`, which closed.
    pub(crate) fn forget(&mut self, file: &Path) {
        self.walks.remove(file);
    }

    /// The entries held.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Loaded-file typechecks run so far.
    pub(crate) fn checks(&self) -> u64 {
        self.checks
    }

    /// Load walks run so far.
    pub(crate) fn walks_run(&self) -> u64 {
        self.walks_run
    }

    /// Loaded files read from disk so far, by a walk or for a digest.
    pub(crate) fn reads(&self) -> u64 {
        self.reads
    }
}

/// A file's content, `None` when it cannot be read as UTF-8.
fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// A file's chained key: its digest, its package, and each of its loads.
fn chain(digest: u128, walked: &WalkedFile, keys: &HashMap<&str, u128>) -> u128 {
    let mut h = Xxh3Default::new();
    h.update(&digest.to_le_bytes());
    put_str(&mut h, &walked.pkg);
    for (module, edge) in &walked.loads {
        chain_edge(&mut h, module, edge, |p| {
            keys.get(p).copied().unwrap_or(UNREADABLE)
        });
    }
    h.digest128()
}

/// One load into a chained key: the string, what it resolved to, and the key
/// of each file it reads, in merge order.
fn chain_edge(h: &mut Xxh3Default, module: &str, edge: &LoadEdge, key: impl Fn(&str) -> u128) {
    put_str(h, module);
    let (tag, pkg): (&[u8], &str) = match edge {
        LoadEdge::File { pkg, .. } => (b"f", pkg),
        LoadEdge::Dir { pkg, .. } => (b"d", pkg),
    };
    h.update(tag);
    put_str(h, pkg);
    for path in edge.files() {
        put_str(h, path);
        h.update(&key(path).to_le_bytes());
    }
}

/// A length-prefixed string, so two adjacent strings never run together.
fn put_str(h: &mut Xxh3Default, s: &str) {
    h.update(&(s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat_changed_at(changed: i128) -> Stat {
        let meta = Meta {
            dev: 1,
            ino: 2,
            mode: 0o100_644,
            len: 3,
            mtime: changed - 5,
            ctime: changed,
        };
        Stat {
            link: meta,
            target: None,
        }
    }

    #[test]
    fn a_stat_is_racy_within_a_tick_of_its_read() {
        let read_at = 100 * 1_000_000_000;
        let tick = RACY.as_nanos() as i128;
        // Changed in the same instant, just before, or after the read: a
        // later rewrite could keep the stat.
        assert!(stat_changed_at(read_at).racy(read_at));
        assert!(stat_changed_at(read_at - tick + 1).racy(read_at));
        assert!(stat_changed_at(read_at + 1).racy(read_at));
        // Safely old: any later change lands on a later timestamp.
        assert!(!stat_changed_at(read_at - tick).racy(read_at));
        // A link's target counts as well as the link.
        let mut linked = stat_changed_at(read_at - 10 * tick);
        assert!(!linked.racy(read_at));
        linked.target = Some(stat_changed_at(read_at).link);
        assert!(linked.racy(read_at));
    }
}
