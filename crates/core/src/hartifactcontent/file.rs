use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{Content, ReadSeek, WalkEntry, WalkEntryKind};

/// [`Content`] backed by a single on-disk file, opened lazily on each read.
///
/// Used to carry a handle to a target's process log inside a failure error
/// without slurping the whole file into memory: the diagnostic reads only the
/// trailing lines it renders, and only when it renders them. The file must
/// outlive the read (process logs persist in the sandbox until the target's
/// next run, so this holds through end-of-run rendering).
#[derive(Debug, Clone)]
pub struct FileContent {
    path: PathBuf,
}

impl FileContent {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open(&self) -> anyhow::Result<std::fs::File> {
        std::fs::File::open(&self.path).with_context(|| format!("opening {}", self.path.display()))
    }
}

impl Content for FileContent {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
        Ok(Box::new(self.open()?))
    }

    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        let name = self.path.file_name().map(PathBuf::from).unwrap_or_default();
        let entry = WalkEntry {
            path: name,
            kind: WalkEntryKind::File {
                data: self.reader()?,
                x: false,
            },
        };
        Ok(Box::new(std::iter::once(Ok(entry))))
    }

    fn hashout(&self) -> anyhow::Result<String> {
        // Logs are never content-addressed; nothing reads this.
        Ok(String::new())
    }

    fn seekable_reader(&self) -> anyhow::Result<Option<Box<dyn ReadSeek + Send>>> {
        Ok(Some(Box::new(self.open()?)))
    }

    fn file_path(&self) -> Option<PathBuf> {
        Some(self.path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn reads_backing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        std::fs::write(&path, b"line1\nline2\n").expect("write");

        let content = FileContent::new(&path);
        assert_eq!(content.file_path().as_deref(), Some(path.as_path()));

        let mut buf = String::new();
        content
            .reader()
            .expect("reader")
            .read_to_string(&mut buf)
            .expect("read");
        assert_eq!(buf, "line1\nline2\n");
    }

    #[test]
    fn missing_file_errors_on_read() {
        let content = FileContent::new("/no/such/log.txt");
        assert!(content.reader().is_err());
    }
}
