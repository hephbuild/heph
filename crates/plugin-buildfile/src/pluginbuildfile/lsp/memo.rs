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
//! The load walk itself is not memoized: it runs on every lookup, through
//! evaluation's resolver, so a load that now resolves elsewhere (a file
//! added to a loaded directory) is seen.
//!
//! An entry holds the `TypeInstanceId`s its typecheck minted, across
//! requests, on purpose: a document typechecked twice against the same
//! entries sees the same record types. I4 governs lint's checks, not this
//! one; an id never reaches any output.

use crate::pluginbuildfile::checks::lint::{Checked, check_loaded, interface_with};
use crate::pluginbuildfile::checks::loads::{LoadEdge, WalkedFile, walk_loads};
use starlark::environment::Globals;
use starlark::typing::Interface;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;
use xxhash_rust::xxh3::{Xxh3Default, xxh3_128};

/// What a stat says of a file: if any of it moves, the content is read and
/// hashed again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    dev: u64,
    ino: u64,
    len: u64,
    modified: Option<SystemTime>,
    ctime: (i64, i64),
}

impl Stat {
    /// `None` when the file cannot be stat'ed: it is then read every time.
    fn of(path: &Path) -> Option<Stat> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(Stat {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            modified: meta.modified().ok(),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

/// One loaded file's memo entry.
struct Entry {
    stat: Option<Stat>,
    /// The content's digest; [`UNREADABLE`] when it could not be read.
    digest: u128,
    /// The chained key the entry was checked under.
    key: u128,
    /// `None`: the file does not parse, so a load of it is left out.
    checked: Option<Checked>,
}

/// The digest of a file that could not be read (or is not UTF-8).
const UNREADABLE: u128 = 0;

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
    /// Loaded-file typechecks run, for `heph/documents` (test support).
    checks: u64,
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
        let walker = hwalk::CachedWalker::disabled();
        let (edges, walk) = walk_loads(root, &walker, pkg, file, modules, patterns);
        let closure: Vec<String> = walk.files.keys().cloned().collect();
        let Ok(order) = walk.dependency_order() else {
            // Nothing typechecks, whatever the files hold: the key is the
            // walk's shape.
            let mut h = Xxh3Default::new();
            h.update(b"unordered");
            for (module, edge) in &edges {
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
            let key = self.refresh(root, path, walked, &keys, globals);
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
        for (module, edge) in &edges {
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
    /// if its stat moved, and typecheck it again if its key did. `keys` holds
    /// the keys of the files it loads.
    fn refresh(
        &mut self,
        root: &Path,
        path: &str,
        walked: &WalkedFile,
        keys: &HashMap<&str, u128>,
        globals: &Globals,
    ) -> u128 {
        let abs = root.join(path);
        let stat = Stat::of(&abs);
        let prev = self.entries.remove(path);
        let mut src = None;
        let digest = match &prev {
            Some(e) if stat.is_some() && e.stat == stat => e.digest,
            _ => {
                src = read(&abs);
                src.as_deref()
                    .map_or(UNREADABLE, |s| xxh3_128(s.as_bytes()))
            }
        };
        let key = chain(digest, walked, keys);
        if let Some(mut e) = prev
            && e.key == key
        {
            e.stat = stat;
            e.digest = digest;
            self.entries.insert(path.to_owned(), e);
            return key;
        }
        // The stat held but a load's key moved: the content is the digest's.
        let src = src.or_else(|| read(&abs));
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

    /// The entries held.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Loaded-file typechecks run so far.
    pub(crate) fn checks(&self) -> u64 {
        self.checks
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
