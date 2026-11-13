use crate::hartifactcontent::{StopRead, WalkEntry, WalkEntryKind};
use anyhow::Context;
use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct PoisonableReader {
    inner: Box<dyn Read>,
    poisoned: Arc<AtomicBool>,
}

impl Read for PoisonableReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "stale tar entry: next() advanced past this entry",
            ));
        }
        self.inner.read(buf)
    }
}

// entries must be declared before _archive: Rust drops fields in declaration order,
// so entries (which borrows from archive's heap) is freed before _archive.
pub struct TarWalker {
    entries: tar::Entries<'static, Box<dyn Read>>,
    _archive: Box<tar::Archive<Box<dyn Read>>>,
    current_poison: Option<Arc<AtomicBool>>,
}

impl TarWalker {
    pub fn new<R: Read + 'static>(from: R) -> anyhow::Result<Self> {
        let boxed: Box<dyn Read> = Box::new(from);
        let mut archive = Box::new(tar::Archive::new(boxed));
        let ptr: *mut tar::Archive<Box<dyn Read>> = &mut *archive;
        // SAFETY: ptr is derived from a Box-allocated archive whose heap address is stable across moves.
        // entries borrows from that heap allocation, not the Box pointer.
        // Field order in TarWalker guarantees entries is dropped before _archive.
        let raw_entries = unsafe { (*ptr).entries()? };
        // SAFETY: transmute extends the lifetime to 'static; safe because _archive (stored as a field)
        // outlives entries due to TarWalker field declaration order and Rust drop order guarantees.
        let entries: tar::Entries<'static, Box<dyn Read>> = unsafe { mem::transmute(raw_entries) };
        Ok(Self {
            entries,
            _archive: archive,
            current_poison: None,
        })
    }
}

impl Iterator for TarWalker {
    type Item = anyhow::Result<WalkEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(poison) = self.current_poison.take() {
            poison.store(true, Ordering::Release);
        }
        loop {
            let entry = match self.entries.next()? {
                Ok(e) => e,
                Err(e) => return Some(Err(e.into())),
            };
            let entry_type = entry.header().entry_type();
            let path: PathBuf = match entry.path() {
                Ok(p) => p.into_owned(),
                Err(e) => return Some(Err(e.into())),
            };
            if entry_type.is_symlink() {
                let target = match entry.link_name() {
                    Ok(Some(p)) => p.into_owned(),
                    Ok(None) => {
                        return Some(Err(anyhow::anyhow!(
                            "tar symlink {:?} has no link name",
                            path
                        )));
                    }
                    Err(e) => return Some(Err(e.into())),
                };
                if target.is_absolute() {
                    return Some(Err(anyhow::anyhow!(
                        "absolute symlink not allowed in tar: {:?} -> {:?}",
                        path,
                        target
                    )));
                }
                return Some(Ok(WalkEntry {
                    path,
                    kind: WalkEntryKind::Symlink { target },
                }));
            }
            if !entry_type.is_file() {
                continue;
            }
            let mode = match entry.header().mode() {
                Ok(m) => m,
                Err(e) => return Some(Err(e.into())),
            };
            let x = mode & 0o111 != 0;
            // Straight off the header we just parsed — no extra read.
            let size = entry.header().size().unwrap_or(0);
            let poison = Arc::new(AtomicBool::new(false));
            self.current_poison = Some(poison.clone());
            // SAFETY: entry borrows from entries which borrows from archive's stable heap allocation.
            // PoisonableReader poisons on next(), preventing reads after the stream advances.
            let reader: Box<dyn Read> = unsafe {
                let entry = mem::transmute::<
                    tar::Entry<'_, Box<dyn Read>>,
                    tar::Entry<'static, Box<dyn Read>>,
                >(entry);
                Box::new(PoisonableReader {
                    inner: Box::new(entry),
                    poisoned: poison,
                })
            };
            return Some(Ok(WalkEntry {
                path,
                kind: WalkEntryKind::File {
                    data: reader,
                    x,
                    size,
                },
            }));
        }
    }
}

enum PackEntry {
    File { source: String, at: String },
    Raw { data: Vec<u8>, at: String, x: bool },
    AppendTar { path: String },
}

pub struct TarPacker {
    entries: Vec<PackEntry>,
}

impl Default for TarPacker {
    fn default() -> Self {
        Self::new()
    }
}

impl TarPacker {
    pub fn new() -> Self {
        Self { entries: vec![] }
    }

    pub fn create_file(&mut self, path: impl Into<String>, at: impl Into<String>) {
        self.entries.push(PackEntry::File {
            source: path.into(),
            at: at.into(),
        });
    }

    pub fn create_raw(&mut self, data: Vec<u8>, at: impl Into<String>, x: bool) {
        self.entries.push(PackEntry::Raw {
            data,
            at: at.into(),
            x,
        });
    }

    pub fn append_tar(&mut self, path: impl Into<String>) {
        self.entries
            .push(PackEntry::AppendTar { path: path.into() });
    }

    pub fn pack<W: Write>(self, to: W) -> anyhow::Result<()> {
        self.pack_with_stop(to, &|| false)
    }

    /// [`pack`](Self::pack), stopping with an error as soon as `stop` reads
    /// true: polled before each entry and on every chunk of an entry's bytes
    /// (see [`StopRead`]), so a cancelled pack reads and writes at most the
    /// rest of one chunk after it is asked to stop. What was written to `to`
    /// by then is a truncated archive; the caller must not publish it.
    ///
    /// Every symlink packed must stay inside the archive: relative, and
    /// resolving from its own directory to a file or a directory of the same
    /// archive (see [`check_links`]). The check runs once every member is
    /// known, after the members are written, so a pack that fails it has
    /// also written a partial archive the caller must not publish.
    pub fn pack_with_stop<W: Write>(self, to: W, stop: &dyn Fn() -> bool) -> anyhow::Result<()> {
        let mut builder = tar::Builder::new(to);
        let mut members: Vec<(String, MemberKind)> = Vec::with_capacity(self.entries.len());

        for entry in self.entries {
            if stop() {
                anyhow::bail!("tar pack stopped: the job was cancelled");
            }
            match entry {
                PackEntry::File { source, at } => {
                    let symlink_meta = std::fs::symlink_metadata(&source)
                        .with_context(|| format!("lstat: {}", source))?;
                    if symlink_meta.file_type().is_symlink() {
                        let target = std::fs::read_link(&source)
                            .with_context(|| format!("readlink: {}", source))?;
                        if target.is_absolute() {
                            anyhow::bail!(
                                "absolute symlink not allowed: {at} -> {} is an absolute \
                                 path; use a relative link, or output the file itself. \
                                 {LINK_RULE}",
                                target.display()
                            );
                        }
                        let mut header = tar::Header::new_gnu();
                        header.set_size(0);
                        header.set_entry_type(tar::EntryType::Symlink);
                        header.set_mode(0o777);
                        builder
                            .append_link(&mut header, &at, &target)
                            .with_context(|| {
                                format!("append symlink {} -> {}", at, target.display())
                            })?;
                        members.push((at, MemberKind::Link(target)));
                    } else {
                        let mut src =
                            File::open(&source).with_context(|| format!("open: {}", source))?;
                        let meta = src.metadata()?;
                        let x = is_executable(&meta);
                        let mut header = tar::Header::new_gnu();
                        header.set_size(meta.len());
                        header.set_mode(if x { 0o755 } else { 0o644 });
                        header.set_cksum();
                        let src = StopRead {
                            inner: &mut src,
                            stop,
                        };
                        builder.append_data(&mut header, &at, src)?;
                        members.push((at, MemberKind::File));
                    }
                }
                PackEntry::Raw { data, at, x } => {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(data.len() as u64);
                    header.set_mode(if x { 0o755 } else { 0o644 });
                    header.set_cksum();
                    builder.append_data(&mut header, &at, Cursor::new(&data))?;
                    members.push((at, MemberKind::File));
                }
                PackEntry::AppendTar { path } => {
                    let src = File::open(&path).with_context(|| format!("open: {}", path))?;
                    let mut archive = tar::Archive::new(src);
                    for entry in archive.entries()? {
                        if stop() {
                            anyhow::bail!("tar pack stopped in {path}: the job was cancelled");
                        }
                        let mut entry = entry?;
                        let header = entry.header().clone();
                        let member = appended_member_path(&entry, &path)?;
                        let entry_type = header.entry_type();
                        let link_name = || -> anyhow::Result<PathBuf> {
                            Ok(entry
                                .link_name()
                                .with_context(|| format!("read link of {member} in {path}"))?
                                .with_context(|| format!("link {member} in {path} has no target"))?
                                .into_owned())
                        };
                        let kind = if entry_type.is_symlink() {
                            MemberKind::Link(link_name()?)
                        } else if entry_type.is_hard_link() {
                            MemberKind::HardLink(link_name()?)
                        } else if entry_type.is_dir() {
                            MemberKind::Dir
                        } else if entry_type.is_file() {
                            MemberKind::File
                        } else {
                            anyhow::bail!(
                                "member {member} in {path} is a {entry_type:?} entry; an \
                                 artifact holds only files, directories and links"
                            );
                        };
                        let entry = StopRead {
                            inner: &mut entry as &mut dyn io::Read,
                            stop,
                        };
                        builder.append(&header, entry)?;
                        members.push((member, kind));
                    }
                }
            }
        }

        check_links(&members)?;
        builder.finish()?;
        Ok(())
    }
}

/// What a member of a packed archive is, as far as [`check_links`] cares.
#[derive(Debug)]
enum MemberKind {
    File,
    Dir,
    /// A symlink, and its target as written.
    Link(PathBuf),
    /// A hardlink (only an appended tar carries one), and the member it
    /// names, relative to the archive root.
    HardLink(PathBuf),
}

/// The last sentence of every refusal of a symlink: the rule it broke.
const LINK_RULE: &str = "A symlink in an artifact must be relative and resolve to a file or \
                         directory packed in the same artifact (the same output group).";

/// How many links one resolution may cross before it is called a loop; the
/// value Linux uses for `SYMLOOP_MAX`.
const MAX_LINK_HOPS: u32 = 40;

/// The path of a member of an appended tar. Refused when it is not UTF-8
/// (two such names could alias in [`check_links`]) or would unpack outside
/// the archive (see [`member_key`]).
fn appended_member_path<R: Read>(
    entry: &tar::Entry<'_, R>,
    tar_path: &str,
) -> anyhow::Result<String> {
    let path = entry
        .path()
        .with_context(|| format!("read member path in {tar_path}"))?;
    let s = path.to_str().with_context(|| {
        format!(
            "member {} in {tar_path} has a path that is not valid UTF-8",
            path.display()
        )
    })?;
    member_key(s).map_err(|why| anyhow::anyhow!("member {s} in {tar_path} {why}"))?;
    Ok(s.to_owned())
}

/// Refuse an archive holding a symlink that does not stay inside it.
///
/// A link is kept only when its target is relative and, resolved lexically
/// from the link's own directory, names a file or a directory of the same
/// archive. Links crossed on the way (as an intermediate component or as the
/// final one) are followed through their own targets, which must stay inside
/// too. A directory counts as "in the archive" when a member lives under it
/// or the archive carries it as an explicit directory entry.
///
/// A link's target is resolved as POSIX resolves a path, on the unpacked
/// archive: every component it looks inside or climbs out of (`..`) must be
/// a directory the archive unpacks — the root always is — so
/// `missing/../f.txt` and `f.txt/../g` are refused rather than read as `f.txt`
/// and `g`, and a `..` after a link climbs from where the link points.
///
/// No member may be packed beneath a link member (`a/l -> ..` and `a/l/x`):
/// unpacking it would write through the link, somewhere this check never
/// looked. An appended hardlink must name a regular file of the archive.
///
/// Lexical, not through the filesystem: what a consumer sees is the unpacked
/// archive, where every member path is a real directory chain. So a link out
/// of the archive (`../../etc/passwd`), to an absolute path, to nothing the
/// archive carries, or round in a loop, fails the pack instead of being
/// handed to a consumer that would follow it.
///
/// Free when the archive has no links: the member list is only indexed once
/// one is found.
fn check_links(members: &[(String, MemberKind)]) -> anyhow::Result<()> {
    use std::collections::{HashMap, HashSet};

    if !members
        .iter()
        .any(|(_, k)| matches!(k, MemberKind::Link(_) | MemberKind::HardLink(_)))
    {
        return Ok(());
    }

    let keys = members
        .iter()
        .map(|(p, _)| member_key(p).map_err(|why| anyhow::anyhow!("member {p} {why}")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut files: HashSet<&str> = HashSet::new();
    // Every directory the unpacked archive has: the root, each explicit
    // directory entry and every strict prefix of a member. Closed under
    // prefixes, which is what lets the indexing below stop early.
    let mut dirs: HashSet<&str> = HashSet::from([""]);
    let mut links: HashMap<&str, &Path> = HashMap::new();
    for (key, (_, kind)) in keys.iter().zip(members) {
        let key: &str = key;
        let mut dir = match kind {
            MemberKind::Dir => Some(key),
            _ => parent_key(key),
        };
        // Deepest first: a directory already present has its ancestors too.
        while let Some(d) = dir {
            if !dirs.insert(d) {
                break;
            }
            dir = parent_key(d);
        }
        match kind {
            MemberKind::File => {
                files.insert(key);
            }
            MemberKind::Link(target) => {
                links.insert(key, target);
            }
            MemberKind::Dir | MemberKind::HardLink(_) => {}
        }
    }

    for (key, (path, kind)) in keys.iter().zip(members) {
        let MemberKind::HardLink(target) = kind else {
            continue;
        };
        let named = target.to_str().and_then(|t| member_key(t).ok());
        anyhow::ensure!(
            named.is_some_and(|n| files.contains(n.as_ref())),
            "hardlink {path} => {} does not name a regular file packed in the same \
             artifact; a hardlink in an artifact must name one of its own files",
            target.display()
        );
        // Once unpacked, a hardlink is a file a symlink may point to.
        files.insert(key);
    }

    for (key, (path, kind)) in keys.iter().zip(members) {
        let MemberKind::Link(target) = kind else {
            continue;
        };
        let key: &str = key;
        if dirs.contains(key) || files.contains(key) {
            let beneath = keys
                .iter()
                .zip(members)
                .find(|(k, _)| k.strip_prefix(key).is_some_and(|r| r.starts_with('/')));
            match beneath {
                Some((_, (under, _))) => anyhow::bail!(
                    "{under} is packed beneath the symlink {path} -> {}, so unpacking it \
                     would write through the link; output the directory the link points \
                     to instead of the link, or the link with nothing beneath it",
                    target.display()
                ),
                None => anyhow::bail!(
                    "symlink {path} -> {} is packed at the same path as a file or \
                     directory; output only one of them",
                    target.display()
                ),
            }
        }
    }

    for (key, (path, kind)) in keys.iter().zip(members) {
        let MemberKind::Link(target) = kind else {
            continue;
        };
        let resolved = resolve_link(key, target, &links, &dirs).map_err(|why| {
            anyhow::anyhow!("symlink {path} -> {} {why}. {LINK_RULE}", target.display())
        })?;
        anyhow::ensure!(
            files.contains(resolved.as_str()) || dirs.contains(resolved.as_str()),
            "symlink {path} -> {} resolves to {resolved}, which is not in the artifact; \
             declare it in `out` too, or fix the link. {LINK_RULE}",
            target.display(),
        );
    }
    Ok(())
}

/// The directory holding the member `key`, `""` being the root; `None` for
/// the root itself.
fn parent_key(key: &str) -> Option<&str> {
    if key.is_empty() {
        return None;
    }
    Some(key.rsplit_once('/').map_or("", |(parent, _)| parent))
}

/// A member path in the one spelling [`check_links`] looks members up by:
/// `/`-joined components, with `.` and empty ones dropped, so `./a//b` and
/// `a/b` are the same member. Borrowed when the path is already in that
/// spelling, as every path heph packs is.
///
/// A path with a `..` component or a leading `/` is refused rather than
/// normalized: it would unpack somewhere else than its key says (`a/../b` is
/// not `a/b`), or outside the archive.
fn member_key(path: &str) -> Result<std::borrow::Cow<'_, str>, &'static str> {
    if path.starts_with('/') {
        return Err("is an absolute path, which would unpack outside the artifact");
    }
    let mut normalized = true;
    for c in path.split('/') {
        match c {
            ".." => return Err("has a `..` component, which would unpack elsewhere"),
            "" | "." => normalized = false,
            _ => {}
        }
    }
    if normalized {
        return Ok(std::borrow::Cow::Borrowed(path));
    }
    let mut key = String::with_capacity(path.len());
    for c in path.split('/').filter(|c| !matches!(*c, "" | ".")) {
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(c);
    }
    Ok(std::borrow::Cow::Owned(key))
}

/// Why a link target cannot be resolved inside the archive.
#[derive(Debug)]
enum Unresolved {
    Absolute,
    Escapes,
    Loops,
    NotUtf8,
    /// Looked inside, or climbed out of, this key, which the archive does not
    /// unpack as a directory.
    NotADir(String),
}

impl std::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absolute => f.write_str(
                "is an absolute path (absolute symlink not allowed); use a relative link, \
                 or output the file itself",
            ),
            Self::Escapes => {
                f.write_str("points above the artifact root (escapes the artifact root)")
            }
            Self::Loops => write!(f, "loops (more than {MAX_LINK_HOPS} levels of symlinks)"),
            Self::NotUtf8 => f.write_str(
                "has a target that is not valid UTF-8, so heph cannot check that it stays \
                 inside the artifact",
            ),
            Self::NotADir(at) => {
                let at = if at.is_empty() { "." } else { at };
                write!(
                    f,
                    "resolves through {at}, which is not in the artifact as a directory; \
                     declare it in `out` too, or fix the link"
                )
            }
        }
    }
}

/// Resolve the link member `link` (a [`member_key`]) to the member key its
/// `target` names, following every link member crossed on the way.
fn resolve_link<'a>(
    link: &str,
    target: &'a Path,
    links: &std::collections::HashMap<&str, &'a Path>,
    dirs: &std::collections::HashSet<&str>,
) -> Result<String, Unresolved> {
    fn to_parent(cur: &mut String) {
        let at = cur.rfind('/').unwrap_or(0);
        cur.truncate(at);
    }

    let mut cur = parent_key(link).unwrap_or("").to_owned();
    let mut pending: Vec<Component<'a>> = target.components().rev().collect();
    let mut hops = 0u32;
    while let Some(c) = pending.pop() {
        let name = match c {
            Component::CurDir => continue,
            Component::RootDir | Component::Prefix(_) => return Err(Unresolved::Absolute),
            Component::ParentDir => None,
            Component::Normal(n) => Some(n.to_str().ok_or(Unresolved::NotUtf8)?),
        };
        // Looking inside `cur`, or climbing out of it: it has to be a
        // directory once unpacked.
        if !dirs.contains(cur.as_str()) {
            return Err(Unresolved::NotADir(cur));
        }
        let Some(name) = name else {
            if cur.is_empty() {
                return Err(Unresolved::Escapes);
            }
            to_parent(&mut cur);
            continue;
        };
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(name);
        if let Some(next) = links.get(cur.as_str()) {
            hops += 1;
            if hops > MAX_LINK_HOPS {
                return Err(Unresolved::Loops);
            }
            to_parent(&mut cur);
            pending.extend(next.components().rev());
        }
    }
    Ok(cur)
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn pack_relative_symlink_included() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"hello").unwrap();
        let link = dir.path().join("link.txt");
        symlink("target.txt", &link).unwrap();

        let mut packer = TarPacker::new();
        packer.create_file(link.to_str().unwrap(), "link.txt");
        packer.create_file(target.to_str().unwrap(), "target.txt");

        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");

        let mut archive = tar::Archive::new(Cursor::new(&buf));
        let mut found_link = false;
        let mut found_target = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let et = entry.header().entry_type();
            if path == std::path::Path::new("link.txt") {
                assert_eq!(et, tar::EntryType::Symlink);
                let link_target = entry.link_name().unwrap().unwrap().into_owned();
                assert_eq!(link_target, std::path::Path::new("target.txt"));
                found_link = true;
            } else if path == std::path::Path::new("target.txt") {
                assert!(et.is_file());
                found_target = true;
            }
        }
        assert!(found_link, "symlink entry missing");
        assert!(found_target, "target file entry missing");
    }

    #[test]
    fn walk_emits_symlink_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"hi").unwrap();
        let link = dir.path().join("link.txt");
        symlink("target.txt", &link).unwrap();

        let mut packer = TarPacker::new();
        packer.create_file(link.to_str().unwrap(), "link.txt");
        packer.create_file(target.to_str().unwrap(), "target.txt");

        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");

        let walker = TarWalker::new(Cursor::new(buf)).expect("walker");
        let mut saw_symlink = false;
        let mut saw_file = false;
        for entry in walker {
            let entry = entry.unwrap();
            match entry.kind {
                WalkEntryKind::Symlink { target } => {
                    assert_eq!(entry.path, std::path::Path::new("link.txt"));
                    assert_eq!(target, std::path::Path::new("target.txt"));
                    saw_symlink = true;
                }
                WalkEntryKind::File { .. } => {
                    assert_eq!(entry.path, std::path::Path::new("target.txt"));
                    saw_file = true;
                }
            }
        }
        assert!(saw_symlink && saw_file);
    }

    #[test]
    fn walk_rejects_absolute_symlink() {
        // Build a tar with an absolute symlink manually (bypassing TarPacker's check).
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            builder
                .append_link(&mut header, "link.txt", "/etc/hosts")
                .unwrap();
            builder.finish().unwrap();
        }
        let walker = TarWalker::new(Cursor::new(buf)).expect("walker");
        let err = walker
            .into_iter()
            .find_map(|r| r.err())
            .expect("expected error");
        assert!(
            format!("{err}").contains("absolute symlink not allowed"),
            "unexpected: {err}"
        );
    }

    /// Counts what reaches it and trips `stop` once `trip_after` bytes have.
    struct TripWriter {
        written: u64,
        trip_after: u64,
        trip: Arc<AtomicBool>,
    }

    impl Write for TripWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written += buf.len() as u64;
            if self.written >= self.trip_after {
                self.trip.store(true, Ordering::Release);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A pack stopped while copying one large file stops within a chunk,
    /// rather than reading the rest of the file.
    #[test]
    fn pack_stops_inside_a_large_file() {
        const SIZE: u64 = 32 << 20;
        const TRIP_AFTER: u64 = 1 << 20;
        let dir = tempfile::tempdir().expect("tempdir");
        let big = dir.path().join("big.bin");
        File::create(&big)
            .and_then(|f| f.set_len(SIZE))
            .expect("sparse file");

        let mut packer = TarPacker::new();
        packer.create_file(big.to_str().expect("utf-8 path"), "big.bin");
        let trip = Arc::new(AtomicBool::new(false));
        let mut w = TripWriter {
            written: 0,
            trip_after: TRIP_AFTER,
            trip: Arc::clone(&trip),
        };
        let stop = || trip.load(Ordering::Acquire);

        packer
            .pack_with_stop(&mut w, &stop)
            .expect_err("a stopped pack fails");
        assert!(
            w.written < 2 * TRIP_AFTER,
            "stopped within a chunk of the trip: wrote {} of {SIZE}",
            w.written
        );
    }

    /// A pack stopped after one entry adds no further entry.
    #[test]
    fn pack_stops_between_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut packer = TarPacker::new();
        for i in 0..20 {
            let p = dir.path().join(format!("f{i}"));
            std::fs::write(&p, b"x").expect("write");
            packer.create_file(p.to_str().expect("utf-8 path"), format!("f{i}"));
        }
        // One small entry is a 512-byte header plus one 512-byte data block.
        let trip = Arc::new(AtomicBool::new(false));
        let mut w = TripWriter {
            written: 0,
            trip_after: 1024,
            trip: Arc::clone(&trip),
        };
        let stop = || trip.load(Ordering::Acquire);

        let err = packer
            .pack_with_stop(&mut w, &stop)
            .expect_err("a stopped pack fails");
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
        // The trip lands on an entry boundary or one entry short of it,
        // depending on how the tar builder splits its writes; either way at
        // most one more entry follows it, not the other eighteen.
        assert!(
            w.written <= 2 * 1024,
            "at most one entry past the stop: wrote {} of {}",
            w.written,
            21 * 1024
        );
    }

    /// Pack `files` (path, body) and `links` (path, target) laid out on disk
    /// under a temp dir, each member packed at its own relative path.
    fn pack_tree(files: &[&str], links: &[Link<'_>]) -> anyhow::Result<Vec<u8>> {
        let dir = tempfile::tempdir()?;
        let mut packer = TarPacker::new();
        let mkdir = |abs: &Path| match abs.parent() {
            Some(p) => std::fs::create_dir_all(p),
            None => Ok(()),
        };
        for rel in files {
            let abs = dir.path().join(rel);
            mkdir(&abs)?;
            std::fs::write(&abs, b"x")?;
            packer.create_file(abs.to_string_lossy(), *rel);
        }
        for (rel, target) in links {
            let abs = dir.path().join(rel);
            mkdir(&abs)?;
            symlink(target, &abs)?;
            packer.create_file(abs.to_string_lossy(), *rel);
        }
        let mut buf = Vec::new();
        packer.pack(&mut buf)?;
        Ok(buf)
    }

    /// A symlink to lay out on disk: (path, target).
    type Link<'a> = (&'a str, &'a str);
    /// A pack case: name, files, links.
    type Case<'a> = (&'a str, &'a [&'a str], &'a [Link<'a>]);

    /// Links that resolve to a file or a directory of the same archive pack.
    #[test]
    fn pack_keeps_links_that_stay_inside() {
        let cases: &[Case<'_>] = &[
            ("sibling file", &["a.txt"], &[("l", "a.txt")]),
            ("file up a level", &["a.txt"], &[("d/l", "../a.txt")]),
            ("dot components", &["d/a.txt"], &[("l", "./d/./a.txt")]),
            ("directory with files", &["d/a.txt"], &[("l", "d")]),
            ("ancestor directory", &["d/a.txt"], &[("d/e/l", "..")]),
            ("the root", &["a.txt"], &[("d/l", "..")]),
            (
                "pnpm-style directory link",
                &["node_modules/.pnpm/x@1/index.js"],
                &[("node_modules/x", ".pnpm/x@1")],
            ),
            (
                "chain of links",
                &["a.txt"],
                &[("l1", "l2"), ("l2", "a.txt")],
            ),
            (
                "through a linked directory",
                &["real/a.txt"],
                &[("dl", "real"), ("l", "dl/a.txt")],
            ),
            (
                "link packed before its target",
                &["z/a.txt"],
                &[("a", "z/a.txt")],
            ),
            (
                "the root of an archive of only links",
                &[],
                &[("d/l", "..")],
            ),
            (
                "`..` after a link climbs from its target (POSIX)",
                &["d/real/a.txt", "d/x"],
                &[("dl", "d/real"), ("l", "dl/../x")],
            ),
            (
                "near-miss: exact a beside a-b",
                &["a", "a-b"],
                &[("l", "a")],
            ),
            ("doubled slash", &["d/a.txt"], &[("l", "d//a.txt")]),
            ("leading ./", &["d/a.txt"], &[("l", "./d/a.txt")]),
            (
                "trailing slash on a directory",
                &["d/a.txt"],
                &[("l", "d/")],
            ),
            (
                "a directory holding only links",
                &["a.txt"],
                &[("d/l1", "../a.txt"), ("x", "d")],
            ),
        ];
        for (name, files, links) in cases {
            pack_tree(files, links).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }

    /// Links that leave the archive, or name nothing in it, fail the pack.
    #[test]
    fn pack_refuses_links_that_leave_the_archive() {
        let cases: &[(Case<'_>, &str)] = &[
            (
                ("out of the root", &["a.txt"], &[("l", "../a.txt")]),
                "escapes",
            ),
            (
                (
                    "out and back in",
                    &["d/a.txt"],
                    &[("d/l", "../../x/d/a.txt")],
                ),
                "escapes",
            ),
            (
                ("dangling", &["a.txt"], &[("l", "b.txt")]),
                "not in the artifact",
            ),
            (
                ("near-miss name", &["d/ab.txt"], &[("l", "d/a")]),
                "not in the artifact",
            ),
            (
                ("directory with no members", &["a.txt"], &[("l", "empty")]),
                "not in the artifact",
            ),
            (("self loop", &["a.txt"], &[("l", "l")]), "loops"),
            (
                ("two-link loop", &["a.txt"], &[("l1", "l2"), ("l2", "l1")]),
                "loops",
            ),
            (
                (
                    "through a link that escapes",
                    &["a.txt"],
                    &[("dl", "../outside"), ("l", "dl/a.txt")],
                ),
                "escapes",
            ),
            (
                (
                    "`..` through a missing directory",
                    &["f.txt"],
                    &[("l", "missing/../f.txt")],
                ),
                "not in the artifact as a directory",
            ),
            (
                (
                    "`..` through a file",
                    &["f.txt", "g"],
                    &[("l", "f.txt/../g")],
                ),
                "not in the artifact as a directory",
            ),
            (
                (
                    "`..` after a link climbs from its target",
                    &["d/real/a.txt", "x"],
                    &[("dl", "d/real"), ("l", "dl/../x")],
                ),
                "not in the artifact",
            ),
            (
                ("near-miss: a-b", &["a-b"], &[("l", "a")]),
                "not in the artifact",
            ),
            (
                ("near-miss: a.txt", &["a.txt"], &[("l", "a")]),
                "not in the artifact",
            ),
            (
                ("near-miss: ab/", &["ab/x"], &[("l", "a")]),
                "not in the artifact",
            ),
            (
                (
                    "member beneath a link member",
                    &["a/f"],
                    &[("a/b/d", ".."), ("a/b/d/l", "../..")],
                ),
                "beneath the symlink",
            ),
        ];
        for ((name, files, links), want) in cases {
            let err = pack_tree(files, links).expect_err(name);
            let msg = format!("{err:#}");
            assert!(msg.contains(want), "{name}: want {want:?} in {msg}");
        }
    }

    /// A symlink carried in by `append_tar` is checked against the whole
    /// archive, not only against the tar it came from.
    #[test]
    fn appended_tar_links_are_checked_against_the_whole_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = dir.path().join("inner.tar");
        {
            let mut builder = tar::Builder::new(File::create(&inner).expect("create"));
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            builder
                .append_link(&mut header, "l", "a.txt")
                .expect("append link");
            builder.finish().expect("finish");
        }

        let alone = {
            let mut p = TarPacker::new();
            p.append_tar(inner.to_str().expect("utf-8"));
            p.pack(Vec::new())
        };
        let err = alone.expect_err("a link to nothing in the archive fails");
        assert!(
            format!("{err:#}").contains("not in the artifact"),
            "{err:#}"
        );

        let mut p = TarPacker::new();
        p.create_raw(b"x".to_vec(), "a.txt", false);
        p.append_tar(inner.to_str().expect("utf-8"));
        p.pack(Vec::new())
            .expect("the link's target is another member of the archive");
    }

    /// A chain crossing exactly [`MAX_LINK_HOPS`] links packs; one more is a
    /// loop.
    #[test]
    fn link_hops_boundary() {
        let chain = |crossed: usize| -> Vec<(String, String)> {
            let mut links: Vec<(String, String)> = (0..crossed)
                .map(|i| (format!("l{i}"), format!("l{}", i + 1)))
                .collect();
            links.push((format!("l{crossed}"), "a.txt".to_owned()));
            links
        };
        let pack = |links: &[(String, String)]| {
            let links: Vec<Link<'_>> = links
                .iter()
                .map(|(l, t)| (l.as_str(), t.as_str()))
                .collect();
            pack_tree(&["a.txt"], &links)
        };
        let max = MAX_LINK_HOPS as usize;
        pack(&chain(max)).expect("40 hops is not a loop");
        let err = pack(&chain(max + 1)).expect_err("41 hops is a loop");
        assert!(format!("{err:#}").contains("loops"), "{err:#}");
    }

    /// A raw tar entry: (name, type, link target).
    type RawEntry<'a> = (&'a str, tar::EntryType, &'a str);

    /// Write a tar of raw entries into `dir`, each name stored byte for byte,
    /// as a foreign tar may hold it (the builder's own path setters normalize
    /// or refuse these).
    fn raw_tar(dir: &Path, entries: &[RawEntry<'_>]) -> String {
        let path = dir.join("raw.tar");
        let mut builder = tar::Builder::new(File::create(&path).expect("create"));
        for (name, kind, link) in entries {
            let body: &[u8] = if *kind == tar::EntryType::Regular {
                b"x"
            } else {
                b""
            };
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            header
                .as_old_mut()
                .name
                .get_mut(..name.len())
                .expect("name fits the header")
                .copy_from_slice(name.as_bytes());
            if !link.is_empty() {
                header.set_link_name(link).expect("link name");
            }
            header.set_cksum();
            builder.append(&header, body).expect("append");
        }
        builder.finish().expect("finish");
        path.to_str().expect("utf-8").to_owned()
    }

    fn pack_appended(entries: &[RawEntry<'_>]) -> anyhow::Result<Vec<u8>> {
        let dir = tempfile::tempdir()?;
        let mut p = TarPacker::new();
        p.append_tar(raw_tar(dir.path(), entries));
        let mut buf = Vec::new();
        p.pack(&mut buf)?;
        Ok(buf)
    }

    /// What an appended tar may carry: members anywhere inside the archive,
    /// directories (an explicit empty one is a valid link target), and
    /// hardlinks to its own files.
    #[test]
    fn appended_tar_members_that_stay_inside() {
        use tar::EntryType::{Directory, Link, Regular, Symlink};
        let cases: &[(&str, &[RawEntry<'_>])] = &[
            (
                "link to an explicit empty directory",
                &[("empty/", Directory, ""), ("l", Symlink, "empty")],
            ),
            (
                "hardlink to a present file",
                &[("a.txt", Regular, ""), ("h", Link, "a.txt")],
            ),
            (
                "a member named ./a//b, linked to as a/b",
                &[("./a//b", Regular, ""), ("l", Symlink, "a/b")],
            ),
        ];
        for (name, entries) in cases {
            pack_appended(entries).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }

    /// What an appended tar may not carry: a member path that unpacks
    /// elsewhere, a hardlink to anything but one of its own files, or an
    /// entry that is not a file, directory or link.
    #[test]
    fn appended_tar_members_that_leave_are_refused() {
        use tar::EntryType::{Fifo, Link, Regular, Symlink};
        let cases: &[(&str, &[RawEntry<'_>], &str)] = &[
            (
                "member named ../x",
                &[("../x", Regular, ""), ("l", Symlink, "x")],
                "`..`",
            ),
            (
                "member named /x",
                &[("/x", Regular, ""), ("l", Symlink, "x")],
                "absolute",
            ),
            ("member named a/../b", &[("a/../b", Regular, "")], "`..`"),
            (
                "hardlink to /etc/passwd",
                &[("a.txt", Regular, ""), ("h", Link, "/etc/passwd")],
                "hardlink h",
            ),
            (
                "hardlink to ../x",
                &[("a.txt", Regular, ""), ("h", Link, "../x")],
                "hardlink h",
            ),
            (
                "hardlink to a missing member",
                &[("a.txt", Regular, ""), ("h", Link, "b.txt")],
                "hardlink h",
            ),
            ("a fifo", &[("p", Fifo, "")], "Fifo"),
            (
                "absolute symlink",
                &[("a.txt", Regular, ""), ("l", Symlink, "/etc/passwd")],
                "absolute symlink not allowed",
            ),
        ];
        for (name, entries, want) in cases {
            let err = pack_appended(entries).expect_err(name);
            let msg = format!("{err:#}");
            assert!(msg.contains(want), "{name}: want {want:?} in {msg}");
        }
    }

    /// A link that does not stay inside names what it resolved to, the
    /// fix, and the rule, with paths printed as paths.
    #[test]
    fn link_refusal_names_the_fix() {
        let err = pack_tree(&["a.txt"], &[("d/l", "../b.txt")]).expect_err("dangling");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "symlink d/l -> ../b.txt resolves to b.txt, which is not in the artifact; \
                 declare it in `out` too, or fix the link. {LINK_RULE}"
            )
        );
    }

    #[test]
    fn pack_absolute_symlink_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("link.txt");
        symlink("/etc/hosts", &link).unwrap();

        let mut packer = TarPacker::new();
        packer.create_file(link.to_str().unwrap(), "link.txt");

        let mut buf = Vec::new();
        let err = packer.pack(&mut buf).expect_err("expected error");
        let msg = format!("{err}");
        assert!(
            msg.contains("absolute symlink not allowed"),
            "unexpected error: {msg}"
        );
    }
}
