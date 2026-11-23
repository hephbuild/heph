//! Opening a `.hephsession`: a recording handed to heph by path or URL.
//!
//! A `.hephsession` is a zstd frame whose content is a sealed `session.db`.
//! There is no container of heph's own: the reader sniffs the leading bytes
//! ([`sniff`]) and takes whichever of three shapes it finds.
//!
//! | Leading bytes | Read as |
//! | --- | --- |
//! | `28 B5 2F FD` (zstd) | decompressed to a temp file, then opened `immutable=1` |
//! | `SQLite format 3\0` | opened `immutable=1` in place |
//! | `PK 03 04` (zip) | the one `*.hephsession` entry inside, by content |
//! | anything else | [`ForeignError::NotASession`] |
//!
//! The zip is what a browser download of a CI artifact gives. Its entry is
//! read by content and never extracted to its name, so a name like
//! `../x.hephsession` reaches nothing. Decompression stops at
//! [`DECOMPRESS_CAP`], so a small file cannot fill the disk.
//!
//! The file is untrusted: it is opened read-only, `immutable=1` (no `-shm`,
//! no locks, nothing written beside it), with `trusted_schema=OFF` and
//! `query_only=1`, and accepted only when its header says it is a heph
//! recording in a format this heph reads and it has a `session` table.

use std::fs::File;
use std::io::{self, Read, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::store;

/// The file extension of an exported recording.
pub const EXTENSION: &str = "hephsession";

/// Where decompression stops: past it the file is refused.
pub const DECOMPRESS_CAP: u64 = 4 << 30;

/// The most a `<ref>` URL may download (`hfetch::Fetcher::https_capped`).
pub const URL_CAP: u64 = 1 << 30;

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const ZIP_MAGIC: [u8; 4] = *b"PK\x03\x04";

/// What a file's leading bytes say it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    Zstd,
    Sqlite,
    Zip,
    Unknown,
}

/// What `lead` (a file's first bytes, at least 16 to tell SQLite) says.
pub fn sniff(lead: &[u8]) -> Sniffed {
    if lead.starts_with(&ZSTD_MAGIC) {
        Sniffed::Zstd
    } else if lead.starts_with(SQLITE_MAGIC) {
        Sniffed::Sqlite
    } else if lead.starts_with(&ZIP_MAGIC) {
        Sniffed::Zip
    } else {
        Sniffed::Unknown
    }
}

/// Why a file cannot be opened as a recording.
#[derive(Debug)]
pub enum ForeignError {
    /// Not a heph session (exit 125). The string says why: what the leading
    /// bytes were, the zip's entries, the decode error, the cap.
    NotASession(String),
    /// A heph recording in a format newer than this heph reads.
    TooNew { found: u32 },
    /// The file could not be read at all (missing, unreadable, disk full
    /// while decompressing).
    Io(anyhow::Error),
}

impl std::fmt::Display for ForeignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotASession(why) => write!(f, "not a heph session: {why}"),
            Self::TooNew { found } => write!(
                f,
                "recorded in session format {found}, newer than this heph reads ({}); upgrade \
                 heph to read it",
                store::FORMAT
            ),
            Self::Io(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ForeignError {}

fn not_a_session(why: impl Into<String>) -> ForeignError {
    ForeignError::NotASession(why.into())
}

/// A foreign recording, open. The temp file a zstd or zip was decompressed
/// into, if any, lives as long as this and is deleted with it.
#[derive(Debug)]
pub struct ForeignFile {
    pub conn: Connection,
    /// What was opened, for messages: the path the user named.
    pub source: PathBuf,
    _decompressed: Option<tempfile::NamedTempFile>,
}

impl ForeignFile {
    /// The connection, and the temp file it reads from (if any) as a path
    /// the caller now removes.
    pub(crate) fn into_parts(self) -> (Connection, Option<PathBuf>) {
        let temp = self
            ._decompressed
            .and_then(|t| match t.into_temp_path().keep() {
                Ok(path) => Some(path),
                Err(e) => {
                    // Removed now; the open connection keeps reading it.
                    tracing::debug!(error = %e, "cannot keep a decompressed session");
                    None
                }
            });
        (self.conn, temp)
    }
}

/// Open `path` as a `.hephsession` (any of the sniffed shapes),
/// decompressing into `tmp_dir` when it must.
pub fn open(path: &Path, tmp_dir: &Path) -> Result<ForeignFile, ForeignError> {
    open_capped(path, tmp_dir, DECOMPRESS_CAP)
}

/// [`open`] with the decompression cap a parameter, so a test can pass it.
pub(crate) fn open_capped(
    path: &Path,
    tmp_dir: &Path,
    cap: u64,
) -> Result<ForeignFile, ForeignError> {
    let io_err = |what: &str, e: io::Error| {
        ForeignError::Io(anyhow::Error::new(e).context(format!("{what} {}", path.display())))
    };
    let mut file = File::open(path).map_err(|e| io_err("open", e))?;
    let lead = read_lead(&mut file).map_err(|e| io_err("read", e))?;
    let decompressed = match sniff(&lead) {
        Sniffed::Sqlite => None,
        Sniffed::Zstd => {
            file.rewind().map_err(|e| io_err("read", e))?;
            Some(decompress(file, tmp_dir, cap)?)
        }
        Sniffed::Zip => Some(from_zip(file, tmp_dir, cap)?),
        Sniffed::Unknown if lead.is_empty() => return Err(not_a_session("the file is empty")),
        Sniffed::Unknown => {
            return Err(not_a_session(format!(
                "it starts with {}, which is none of a zstd frame, a SQLite file or a zip",
                describe_lead(&lead)
            )));
        }
    };
    let db = decompressed.as_ref().map_or(path, |t| t.path());
    let conn = open_db(db)?;
    Ok(ForeignFile {
        conn,
        source: path.to_path_buf(),
        _decompressed: decompressed,
    })
}

/// Up to the first 16 bytes of `r`: fewer only at end of file.
fn read_lead(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut lead = Vec::with_capacity(SQLITE_MAGIC.len());
    r.take(SQLITE_MAGIC.len() as u64).read_to_end(&mut lead)?;
    Ok(lead)
}

/// The leading bytes as hex, and as text when they are printable.
fn describe_lead(lead: &[u8]) -> String {
    let hex = lead
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    if lead
        .iter()
        .take(8)
        .all(|b| b.is_ascii_graphic() || *b == b' ')
    {
        let text: String = lead.iter().take(8).map(|b| char::from(*b)).collect();
        format!("`{hex}` ({text:?})")
    } else {
        format!("`{hex}`")
    }
}

/// A temp file in `tmp_dir`, deleted on drop.
fn temp_in(tmp_dir: &Path) -> Result<tempfile::NamedTempFile, ForeignError> {
    std::fs::create_dir_all(tmp_dir)
        .and_then(|()| tempfile::NamedTempFile::new_in(tmp_dir))
        .map_err(|e| {
            ForeignError::Io(
                anyhow::Error::new(e)
                    .context(format!("create a temp file in {}", tmp_dir.display())),
            )
        })
}

/// Copy at most `cap` bytes of `src` into a new temp file; more is refused.
/// A read error (a truncated or corrupt frame) is the file's, not the disk's.
fn copy_capped(
    mut src: impl Read,
    tmp_dir: &Path,
    cap: u64,
    what: &str,
) -> Result<tempfile::NamedTempFile, ForeignError> {
    let mut temp = temp_in(tmp_dir)?;
    let mut out = io::BufWriter::new(temp.as_file_mut());
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(not_a_session(format!("{what}: {e}"))),
        };
        total += n as u64;
        if total > cap {
            return Err(not_a_session(format!(
                "{what}: it decompresses past the {} GiB cap",
                cap >> 30
            )));
        }
        let chunk = buf.get(..n).unwrap_or_default();
        out.write_all(chunk).map_err(|e| {
            ForeignError::Io(anyhow::Error::new(e).context("write the decompressed recording"))
        })?;
    }
    out.flush().map_err(|e| {
        ForeignError::Io(anyhow::Error::new(e).context("write the decompressed recording"))
    })?;
    drop(out);
    Ok(temp)
}

/// A zstd stream decompressed into a temp file, capped. Its content must be
/// a SQLite file: a zstd of a zip, or of another zstd, is refused by the
/// header check that follows.
fn decompress(
    src: impl Read,
    tmp_dir: &Path,
    cap: u64,
) -> Result<tempfile::NamedTempFile, ForeignError> {
    let decoder = zstd::stream::read::Decoder::new(src)
        .map_err(|e| not_a_session(format!("cannot decode the zstd frame: {e}")))?;
    copy_capped(decoder, tmp_dir, cap, "cannot decode the zstd frame")
}

/// The one `*.hephsession` entry of a zip, read by content into a temp file.
fn from_zip(file: File, tmp_dir: &Path, cap: u64) -> Result<tempfile::NamedTempFile, ForeignError> {
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| not_a_session(format!("cannot read the zip: {e}")))?;
    // Raw names, by index: a name is only ever matched and quoted, never a
    // path, and an entry is then read by its index.
    let names: Vec<String> = archive
        .file_names_raw()
        .map(|raw| String::from_utf8_lossy(raw).into_owned())
        .collect();
    let suffix = format!(".{EXTENSION}");
    let sessions: Vec<(usize, &String)> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.ends_with('/') && n.ends_with(&suffix))
        .collect();
    let (index, name) = match sessions.as_slice() {
        [one] => *one,
        [] => {
            return Err(not_a_session(format!(
                "the zip holds no *{suffix} entry (it holds: {})",
                list(&names)
            )));
        }
        several => {
            let found: Vec<&String> = several.iter().map(|(_, n)| *n).collect();
            return Err(not_a_session(format!(
                "the zip holds {} *{suffix} entries, not one: {}; extract the one to open",
                found.len(),
                list(&found)
            )));
        }
    };
    let what = format!("zip entry {name:?}");
    let mut entry = archive
        .by_index(index)
        .map_err(|e| not_a_session(format!("cannot read {what}: {e}")))?;
    let lead = read_lead(&mut entry).map_err(|e| not_a_session(format!("{what}: {e}")))?;
    // The entry is itself a `.hephsession`: a zstd frame, or a bare SQLite
    // file. Never a zip again. The lead already read goes first.
    let rest = io::Cursor::new(lead.clone()).chain(entry);
    match sniff(&lead) {
        Sniffed::Zstd => decompress(rest, tmp_dir, cap),
        Sniffed::Sqlite => copy_capped(rest, tmp_dir, cap, &what),
        Sniffed::Zip | Sniffed::Unknown => Err(not_a_session(format!(
            "{what} starts with {}, which is neither a zstd frame nor a SQLite file",
            describe_lead(&lead)
        ))),
    }
}

/// Names for a message, quoted, sorted, at most ten.
fn list(names: &[impl AsRef<str>]) -> String {
    if names.is_empty() {
        return "nothing".to_string();
    }
    let mut sorted: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
    sorted.sort_unstable();
    let shown: Vec<String> = sorted.iter().take(10).map(|n| format!("{n:?}")).collect();
    match sorted.len() {
        n if n > 10 => format!("{}, and {} more", shown.join(", "), n - 10),
        _ => shown.join(", "),
    }
}

/// What the first 100 bytes of a SQLite file say, beyond [`store::Header`]:
/// whether the file is shorter than the database it describes.
fn truncation(path: &Path) -> io::Result<Option<(u64, u64)>> {
    let mut head = [0u8; 100];
    let mut f = File::open(path)?;
    f.read_exact(&mut head)?;
    let be32 = |at: usize| {
        head.get(at..at + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map_or(0, u32::from_be_bytes)
    };
    let page_size = match head.get(16..18) {
        Some(&[1, 0]) => 65_536,
        Some(&[hi, lo]) => u64::from(u16::from_be_bytes([hi, lo])),
        _ => 0,
    };
    // The in-header page count is valid only when the change counter
    // matches the version-valid-for number (SQLite file format, 1.3.7).
    let pages = u64::from(be32(28));
    if page_size == 0 || pages == 0 || be32(24) != be32(92) {
        return Ok(None);
    }
    let want = page_size * pages;
    let have = f.metadata()?.len();
    Ok((have < want).then_some((have, want)))
}

/// Open the SQLite file at `db` as an untrusted, read-only recording.
fn open_db(db: &Path) -> Result<Connection, ForeignError> {
    let header = store::read_header(db)
        .map_err(|e| ForeignError::Io(anyhow::Error::new(e).context("read the recording")))?
        .ok_or_else(|| not_a_session("it is not a SQLite file"))?;
    if !header.is_heph() {
        return Err(not_a_session(
            "a SQLite file, but not a heph recording (its application_id is not heph's)",
        ));
    }
    if header.format > store::FORMAT {
        return Err(ForeignError::TooNew {
            found: header.format,
        });
    }
    if header.format == 0 {
        return Err(not_a_session("a heph recording with no format (0)"));
    }
    if let Some((have, want)) = truncation(db)
        .map_err(|e| ForeignError::Io(anyhow::Error::new(e).context("read the recording")))?
    {
        return Err(not_a_session(format!(
            "it is truncated: {have} of {want} bytes (a cut download?)"
        )));
    }
    // Read-only and `immutable=1`, whatever the header's journal mode: a
    // foreign file is never written, and nothing is created beside it.
    let conn = store::open_sealed(db)
        .map_err(|e| not_a_session(format!("cannot open it as SQLite: {e}")))?;
    conn.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=1;")
        .map_err(|e| not_a_session(format!("cannot open it as SQLite: {e}")))?;
    let has_session: bool = conn
        .query_row(
            "SELECT count(*) = 1 FROM sqlite_schema WHERE type = 'table' AND name = 'session'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| not_a_session(format!("cannot read its schema: {e}")))?;
    if !has_session {
        return Err(not_a_session(
            "a SQLite file with heph's application_id but no `session` table",
        ));
    }
    Ok(conn)
}

#[cfg(test)]
#[path = "hephsession_tests.rs"]
mod tests;
