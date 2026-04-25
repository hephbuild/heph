use std::fs::File;
use std::io::{self, Cursor};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

enum Entry {
    File { source: String, at: String },
    Raw { data: Vec<u8>, at: String, x: bool },
    AppendTar { path: String },
}

pub struct TarPacker {
    entries: Vec<Entry>,
}

impl TarPacker {
    pub fn new() -> Self {
        Self { entries: vec![] }
    }

    pub fn create_file(&mut self, path: impl Into<String>, at: impl Into<String>) {
        self.entries.push(Entry::File { source: path.into(), at: at.into() });
    }

    pub fn create_raw(&mut self, data: Vec<u8>, at: impl Into<String>) {
        self.entries.push(Entry::Raw { data, at: at.into(), x: false });
    }

    pub fn append_tar(&mut self, path: impl Into<String>) {
        self.entries.push(Entry::AppendTar { path: path.into() });
    }

    pub fn pack(self, to: &Path) -> anyhow::Result<()> {
        let out = File::create(to)?;
        let mut builder = tar::Builder::new(out);

        for entry in self.entries {
            match entry {
                Entry::File { source, at } => {
                    let mut src = File::open(&source)?;
                    let meta = src.metadata()?;
                    let x = is_executable(&meta);
                    let mut header = tar::Header::new_gnu();
                    header.set_size(meta.len());
                    header.set_mode(if x { 0o755 } else { 0o644 });
                    header.set_cksum();
                    builder.append_data(&mut header, &at, &mut src)?;
                }
                Entry::Raw { data, at, x } => {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(data.len() as u64);
                    header.set_mode(if x { 0o755 } else { 0o644 });
                    header.set_cksum();
                    builder.append_data(&mut header, &at, Cursor::new(&data))?;
                }
                Entry::AppendTar { path } => {
                    let src = File::open(&path)?;
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
