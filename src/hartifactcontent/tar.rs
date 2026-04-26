use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::mem;
use std::path::PathBuf;
use crate::hartifactcontent::WalkEntry;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// entries must be declared before _archive: Rust drops fields in declaration order,
// so entries (which borrows from archive's heap) is freed before _archive.
pub struct TarWalker {
    entries: tar::Entries<'static, Box<dyn Read>>,
    _archive: Box<tar::Archive<Box<dyn Read>>>,
}

impl TarWalker {
    pub fn new<R: Read + 'static>(from: R) -> anyhow::Result<Self> {
        let boxed: Box<dyn Read> = Box::new(from);
        let mut archive = Box::new(tar::Archive::new(boxed));
        // SAFETY: archive is Box-allocated so its heap address is stable across moves.
        // entries borrows from that heap allocation, not the Box pointer.
        // Field order above guarantees entries is dropped before _archive.
        let entries: tar::Entries<'static, Box<dyn Read>> = unsafe {
            let ptr: *mut tar::Archive<Box<dyn Read>> = &mut *archive;
            mem::transmute((*ptr).entries()?)
        };
        Ok(Self { entries, _archive: archive })
    }
}

impl Iterator for TarWalker {
    type Item = anyhow::Result<WalkEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut entry = match self.entries.next()? {
                Ok(e) => e,
                Err(e) => return Some(Err(e.into())),
            };
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path: PathBuf = match entry.path() {
                Ok(p) => p.into_owned(),
                Err(e) => return Some(Err(e.into())),
            };
            let mode = match entry.header().mode() {
                Ok(m) => m,
                Err(e) => return Some(Err(e.into())),
            };
            let x = mode & 0o111 != 0;
            let mut data = Vec::new();
            if let Err(e) = entry.read_to_end(&mut data) {
                return Some(Err(e.into()));
            }
            return Some(Ok(WalkEntry { path, data, x }));
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
        self.entries.push(PackEntry::File { source: path.into(), at: at.into() });
    }

    pub fn create_raw(&mut self, data: Vec<u8>, at: impl Into<String>, x: bool) {
        self.entries.push(PackEntry::Raw { data, at: at.into(), x });
    }

    pub fn append_tar(&mut self, path: impl Into<String>) {
        self.entries.push(PackEntry::AppendTar { path: path.into() });
    }

    pub fn pack<W: Write>(self, to: W) -> anyhow::Result<()> {
        let mut builder = tar::Builder::new(to);

        for entry in self.entries {
            match entry {
                PackEntry::File { source, at } => {
                    let mut src = File::open(&source)
                        .map_err(|err| anyhow::anyhow!("open: {}: {}", source, err))?;
                    let meta = src.metadata()?;
                    let x = is_executable(&meta);
                    let mut header = tar::Header::new_gnu();
                    header.set_size(meta.len());
                    header.set_mode(if x { 0o755 } else { 0o644 });
                    header.set_cksum();
                    builder.append_data(&mut header, &at, &mut src)?;
                }
                PackEntry::Raw { data, at, x } => {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(data.len() as u64);
                    header.set_mode(if x { 0o755 } else { 0o644 });
                    header.set_cksum();
                    builder.append_data(&mut header, &at, Cursor::new(&data))?;
                }
                PackEntry::AppendTar { path } => {
                    let src = File::open(&path)
                        .map_err(|err| anyhow::anyhow!("open: {}: {}", path, err))?;
                    let mut archive = tar::Archive::new(src);
                    for entry in archive.entries()? {
                        let mut entry = entry?;
                        let header = entry.header().clone();
                        builder.append(&header, &mut entry as &mut dyn io::Read)?;
                    }
                }
            }
        }

        builder.finish()?;
        Ok(())
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}
