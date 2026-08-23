//! Per-directory codegen provenance registry — the `.hephgen` file.
//!
//! A `codegen = "copy"` target writes generated files into the source tree.
//! Those files must never be re-sourced as raw input (a `glob()` that picked one
//! up would double-source the generated content, and for a target whose own
//! package is globbed it would fold the output back into its input hash). So
//! every such file needs a provenance record saying "heph wrote this, and this
//! target owns it".
//!
//! The record lives in a `.hephgen` file **in the same directory as the files it
//! describes**, and that placement is the whole design. The mechanism it
//! replaces — a `user.heph.codegen` xattr on each file — failed because the
//! record's lifetime was decoupled from the file's: `git checkout`, `tar`,
//! `rsync` without `-X`, `cp` without `-p`, an editor's atomic save and any
//! filesystem without user xattrs all preserve the bytes and drop the metadata,
//! and a dropped record does not fail — it silently turns a build output back
//! into a build input. A sibling file moves, copies and dies with the directory
//! it describes, so `cp -r`, `mv`, `tar`, `rsync`, docker `COPY` and
//! `git clean -xd` all keep the two in step.
//!
//! # Format
//!
//! ```text
//! heph-codegen 1
//! bar.pb.go\t//pkg:gen\th=9a1c0f4e
//! foo.pb.go\t//pkg:gen\th=8f2a77b1\tprev=1c04dd90
//! gen\t//pkg:gendir\t->../../.heph3/cache/blob/…
//! ```
//!
//! Tab-separated, sorted by name, describing **only its own directory** — never
//! a nested path — so directory-local reasoning holds and moving a directory
//! moves its truth with it.
//!
//! - **Column 1 is always the entry name**, and stays so in every future
//!   version. That is what lets an older binary read a newer file: it degrades
//!   to [`Record::Opaque`] (the name is known to be generated, the content
//!   cannot be verified) instead of failing open and mis-sourcing the file.
//! - Column 2 is the owning target's addr, so a lookup answers *who* owns the
//!   path — which is what lets an `in_place` target tell "a file some other
//!   codegen target owns" from "a file I am about to rewrite".
//! - `h=` is the [`crate::cached_walker::file_hashout`] of the bytes heph wrote
//!   (content plus the exec bit). It is what makes a stale record safe: if the
//!   file no longer holds those bytes it is not heph's file, whatever the
//!   registry says, and it goes back to being source.
//! - `prev=` names the bytes the file held *before* an in-flight rewrite, and
//!   exists only inside that window (see the ordering rules on the writer).
//! - `->` replaces the hash for a symlink entry (a `DirPath` codegen output,
//!   materialized as a link into the heph home).
//!
//! The reader is deliberately lenient: an unparseable line is skipped, an
//! unreadable or absent file is an empty registry. A registry that cannot be
//! read costs a file its provenance, which the owning target restores the next
//! time it runs.

use borsh::{BorshDeserialize, BorshSerialize};
use std::path::Path;

/// The per-directory registry file name.
///
/// It starts with `.heph`, which is what keeps it out of every glob: the fs
/// provider already skips entries whose name has that prefix as engine-internal.
pub const REGISTRY_NAME: &str = ".hephgen";

/// First-line magic, followed by the format version.
const MAGIC: &str = "heph-codegen";

/// The format version this binary writes. Bump only for a change a previous
/// version would *misread*; additive columns do not need one, because column 1
/// is fixed and unknown columns are ignored.
const VERSION: u32 = 1;

/// What is known about the bytes an entry's name is expected to hold.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Record {
    /// A regular file, with the hash of the bytes heph wrote. `prev` is the hash
    /// the file held before an in-flight rewrite and is accepted for as long as
    /// it is present.
    File { hash: String, prev: Option<String> },
    /// A symlink output; the link target rather than a content hash.
    Symlink { target: String },
    /// A record written by a newer format version than this binary understands:
    /// the name is generated, the content cannot be verified.
    Opaque,
}

/// One registered generated entry in a directory.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Entry {
    /// File name within the registry's own directory. Never a path.
    pub name: String,
    /// Formatted addr of the `codegen = "copy"` target that owns it.
    pub owner: String,
    pub record: Record,
}

impl Entry {
    /// Whether `hashout` (a [`crate::cached_walker::file_hashout`]) is content
    /// heph is known to have written here.
    ///
    /// A symlink or opaque record accepts unconditionally — neither carries a
    /// content hash, and "generated" is the safe answer when provenance is known
    /// but content identity is not: an unverifiable *hide* leaves a stale file
    /// out of the build, where an unverifiable *reveal* would compile it.
    pub fn accepts(&self, hashout: &str) -> bool {
        match &self.record {
            Record::File { hash, prev } => hash == hashout || prev.as_deref() == Some(hashout),
            Record::Symlink { .. } | Record::Opaque => true,
        }
    }
}

/// The parsed `.hephgen` of one directory. Entries are sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Registry {
    entries: Vec<Entry>,
}

impl Registry {
    /// An empty registry — the answer for every directory with no `.hephgen`.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The record for `name`, if this directory registers it.
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .and_then(|i| self.entries.get(i))
    }

    /// Read and parse `<dir>/.hephgen`. A missing or unreadable file is an empty
    /// registry — never an error, so a walk can never break on one.
    pub fn load(dir: &Path) -> Self {
        match std::fs::read_to_string(dir.join(REGISTRY_NAME)) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::empty(),
        }
    }

    /// Parse the registry text. Lenient by construction: a missing or unknown
    /// magic yields an empty registry, and an unparseable line is skipped rather
    /// than poisoning the rest.
    pub fn parse(text: &str) -> Self {
        let mut lines = text.lines();
        let Some(header) = lines.next() else {
            return Self::empty();
        };
        let mut header = header.split_whitespace();
        if header.next() != Some(MAGIC) {
            return Self::empty();
        }
        // A newer version keeps column 1 (the name) and column 2 (the owner) by
        // format promise; everything past that is read as opaque.
        let version: u32 = header.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let known = version == VERSION;

        let mut entries: Vec<Entry> = lines
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| {
                let mut cols = line.split('\t');
                let name = cols.next()?;
                let owner = cols.next()?;
                if name.is_empty() || owner.is_empty() {
                    return None;
                }
                let record = if known {
                    parse_record(cols)
                } else {
                    Record::Opaque
                };
                Some(Entry {
                    name: name.to_string(),
                    owner: owner.to_string(),
                    record,
                })
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries.dedup_by(|a, b| a.name == b.name);
        Self { entries }
    }

    /// Render to the on-disk form. Deterministic: sorted, so an unchanged
    /// registry renders to identical bytes and needs no write.
    pub fn render(&self) -> String {
        let mut out = format!("{MAGIC} {VERSION}\n");
        for e in &self.entries {
            out.push_str(&e.name);
            out.push('\t');
            out.push_str(&e.owner);
            match &e.record {
                Record::File { hash, prev } => {
                    out.push_str("\th=");
                    out.push_str(hash);
                    if let Some(prev) = prev {
                        out.push_str("\tprev=");
                        out.push_str(prev);
                    }
                }
                Record::Symlink { target } => {
                    out.push_str("\t->");
                    out.push_str(target);
                }
                // An opaque record came from a newer version and its columns are
                // not understood, so they cannot be re-rendered. Keeping the name
                // and owner preserves the provenance this binary can act on.
                Record::Opaque => {}
            }
            out.push('\n');
        }
        out
    }

    /// Insert or replace the record for `entry.name`.
    pub fn upsert(&mut self, entry: Entry) {
        match self.entries.binary_search_by(|e| e.name.cmp(&entry.name)) {
            Ok(i) => {
                if let Some(slot) = self.entries.get_mut(i) {
                    *slot = entry;
                }
            }
            Err(i) => self.entries.insert(i, entry),
        }
    }

    /// Drop the entry for `name`, if any.
    pub fn remove(&mut self, name: &str) {
        if let Ok(i) = self.entries.binary_search_by(|e| e.name.as_str().cmp(name)) {
            self.entries.remove(i);
        }
    }

    /// Drop every entry owned by `owner` whose name is not in `keep`.
    ///
    /// This is how a target that stopped emitting a file cleans up after itself:
    /// the rewrite that follows a run states exactly what the target emits now,
    /// and anything else it used to own stops being registered. Entries owned by
    /// *other* targets are untouched — one directory can serve several.
    pub fn retain_owned(&mut self, owner: &str, keep: &[String]) {
        self.entries
            .retain(|e| e.owner != owner || keep.contains(&e.name));
    }

    /// Clear every in-flight `prev=` hash. Called once the rewrites that needed
    /// the widened acceptance have landed.
    pub fn clear_prev(&mut self) -> bool {
        let mut changed = false;
        for e in &mut self.entries {
            if let Record::File { prev, .. } = &mut e.record
                && prev.take().is_some()
            {
                changed = true;
            }
        }
        changed
    }
}

/// Parse the trailing columns of a known-version line into a [`Record`].
fn parse_record<'a>(cols: impl Iterator<Item = &'a str>) -> Record {
    let mut hash = String::new();
    let mut prev = None;
    for col in cols {
        if let Some(target) = col.strip_prefix("->") {
            return Record::Symlink {
                target: target.to_string(),
            };
        } else if let Some(h) = col.strip_prefix("h=") {
            hash = h.to_string();
        } else if let Some(p) = col.strip_prefix("prev=") {
            prev = Some(p.to_string());
        }
        // Unknown columns are ignored: additive fields must not need a version
        // bump to stay readable.
    }
    Record::File { hash, prev }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, owner: &str, hash: &str) -> Entry {
        Entry {
            name: name.to_string(),
            owner: owner.to_string(),
            record: Record::File {
                hash: hash.to_string(),
                prev: None,
            },
        }
    }

    #[test]
    fn round_trips_files_symlinks_and_prev() {
        let mut r = Registry::empty();
        r.upsert(file("foo.go", "//pkg:gen", "aaa"));
        r.upsert(Entry {
            name: "bar.go".to_string(),
            owner: "//pkg:gen".to_string(),
            record: Record::File {
                hash: "bbb".to_string(),
                prev: Some("ccc".to_string()),
            },
        });
        r.upsert(Entry {
            name: "gen".to_string(),
            owner: "//pkg:dir".to_string(),
            record: Record::Symlink {
                target: "../.heph3/x".to_string(),
            },
        });

        let parsed = Registry::parse(&r.render());
        assert_eq!(parsed, r, "render → parse must be lossless");
        // Sorted by name, so the rendered bytes are stable across insertion order.
        assert_eq!(
            parsed
                .entries()
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["bar.go", "foo.go", "gen"]
        );
    }

    #[test]
    fn accepts_current_and_in_flight_hashes() {
        let e = Entry {
            name: "a".to_string(),
            owner: "//p:t".to_string(),
            record: Record::File {
                hash: "new".to_string(),
                prev: Some("old".to_string()),
            },
        };
        // Both sides of a rewrite window are heph's own bytes.
        assert!(e.accepts("new"));
        assert!(e.accepts("old"));
        // Anything else is not, and goes back to being source.
        assert!(!e.accepts("other"));
    }

    /// A file written by a future version stays a *hide*, not a reveal: the name
    /// and owner survive by format promise, and the unreadable columns degrade to
    /// `Opaque` rather than to "not generated".
    #[test]
    fn future_version_degrades_to_names_only() {
        let text = "heph-codegen 99\nfoo.go\t//pkg:gen\tsha3=deadbeef\textra=1\n";
        let r = Registry::parse(text);
        let e = r.get("foo.go").expect("name survives a version bump");
        assert_eq!(e.owner, "//pkg:gen");
        assert_eq!(e.record, Record::Opaque);
        assert!(e.accepts("whatever-is-on-disk"));
    }

    #[test]
    fn foreign_or_damaged_files_read_as_empty() {
        assert!(Registry::parse("").is_empty());
        assert!(Registry::parse("# just a comment\nfoo\tbar\n").is_empty());
        // A truncated line inside a good file is skipped, not fatal.
        let r = Registry::parse("heph-codegen 1\nnoowner\nfoo.go\t//pkg:gen\th=aa\n");
        assert_eq!(r.entries().len(), 1);
        assert!(r.get("foo.go").is_some());
    }

    #[test]
    fn retain_owned_drops_only_its_own_stale_entries() {
        let mut r = Registry::empty();
        r.upsert(file("mine_kept.go", "//pkg:a", "1"));
        r.upsert(file("mine_stale.go", "//pkg:a", "2"));
        r.upsert(file("theirs.go", "//pkg:b", "3"));

        r.retain_owned("//pkg:a", &["mine_kept.go".to_string()]);

        assert!(r.get("mine_kept.go").is_some());
        assert!(
            r.get("mine_stale.go").is_none(),
            "own stale entry is dropped"
        );
        assert!(
            r.get("theirs.go").is_some(),
            "another owner's entry is untouched"
        );
    }

    #[test]
    fn clear_prev_narrows_acceptance() {
        let mut r = Registry::empty();
        r.upsert(Entry {
            name: "a".to_string(),
            owner: "//p:t".to_string(),
            record: Record::File {
                hash: "new".to_string(),
                prev: Some("old".to_string()),
            },
        });
        assert!(r.clear_prev());
        assert!(!r.clear_prev(), "idempotent once cleared");
        let e = r.get("a").expect("entry");
        assert!(e.accepts("new"));
        assert!(!e.accepts("old"), "the rewrite window is closed");
    }
}
