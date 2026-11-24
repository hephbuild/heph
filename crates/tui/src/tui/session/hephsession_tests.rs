//! The sniffer and the foreign open, against the spec's adversarial table
//! ("Adversarial inputs for `<ref>` and the sniffer").

use super::*;
use std::io::Write as _;

/// The committed format-1 recording: a real sealed `session.db`.
fn fixture() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tui/session/testdata/format-1")
        .join(store::FILE);
    std::fs::read(&path).expect("read the format-1 fixture")
}

fn zstd_of(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, 3).expect("compress")
}

/// A stored (uncompressed) zip of `entries`, and one deflated, as a CI
/// artifact download is.
fn zip_of(entries: &[(&str, &[u8])], method: zip::CompressionMethod) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(method);
    for (name, bytes) in entries {
        w.start_file(*name, opts).expect("start entry");
        w.write_all(bytes).expect("write entry");
    }
    w.finish().expect("finish zip").into_inner()
}

/// A workspace for one test: the file under test and the temp directory.
struct Case {
    dir: tempfile::TempDir,
}

impl Case {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn tmp(&self) -> PathBuf {
        self.dir.path().join("tmp")
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, bytes).expect("write the input");
        path
    }

    fn open(&self, bytes: &[u8]) -> Result<ForeignFile, ForeignError> {
        open(&self.write("in.hephsession", bytes), &self.tmp())
    }

    /// Every file under the case's directory, relative, sorted.
    fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![self.dir.path().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).expect("read dir") {
                let p = e.expect("entry").path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(self.dir.path()).expect("under the case");
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        out
    }
}

fn session_id(f: &ForeignFile) -> String {
    f.conn
        .query_row("SELECT id FROM session", [], |r| r.get(0))
        .expect("read the session row")
}

fn not_a_session_why(res: Result<ForeignFile, ForeignError>) -> String {
    match res {
        Err(ForeignError::NotASession(why)) => why,
        other => panic!("expected not-a-session, got {other:?}"),
    }
}

#[test]
fn sniff_reads_the_leading_bytes() {
    let cases: [(&[u8], Sniffed); 9] = [
        (&[0x28, 0xB5, 0x2F, 0xFD, 0x00], Sniffed::Zstd),
        (b"SQLite format 3\0rest", Sniffed::Sqlite),
        (b"PK\x03\x04rest", Sniffed::Zip),
        (b"", Sniffed::Unknown),
        (b"<!DOCTYPE html>", Sniffed::Unknown),
        // Near misses.
        (b"SQLite format 3", Sniffed::Unknown),
        (b"SQLite format 2\0", Sniffed::Unknown),
        (b"PK\x05\x06", Sniffed::Unknown),
        (&[0x28, 0xB5, 0x2F], Sniffed::Unknown),
    ];
    for (lead, want) in cases {
        assert_eq!(sniff(lead), want, "{lead:?}");
    }
}

/// The three shapes open to the same recording, and the temp copy a
/// decompression made goes when the recording is dropped.
#[test]
fn every_shape_opens_and_leaves_nothing_behind() {
    let db = fixture();
    let want = {
        let case = Case::new();
        session_id(&case.open(&db).expect("bare SQLite"))
    };
    let shapes: [(&str, Vec<u8>); 4] = [
        ("zstd", zstd_of(&db)),
        (
            "zip of zstd, deflated",
            zip_of(
                &[("heph-x.hephsession", &zstd_of(&db))],
                zip::CompressionMethod::Deflated,
            ),
        ),
        (
            "zip of SQLite, stored",
            zip_of(
                &[("sessions/a.hephsession", &db), ("README.txt", b"hi")],
                zip::CompressionMethod::Stored,
            ),
        ),
        ("bare SQLite", db.clone()),
    ];
    for (label, bytes) in shapes {
        let case = Case::new();
        let opened = case.open(&bytes).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(session_id(&opened), want, "{label}");
        drop(opened);
        assert_eq!(
            case.files(),
            ["in.hephsession"],
            "{label}: left files behind"
        );
    }
}

/// Opening never writes beside the file: no `-shm`, `-wal` or journal, even
/// with the source read-only.
#[test]
fn a_foreign_file_is_never_written() {
    use std::os::unix::fs::PermissionsExt as _;
    let case = Case::new();
    let path = case.write("ro.hephsession", &fixture());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    let opened = open(&path, &case.tmp()).expect("open read-only");
    let err = opened
        .conn
        .execute("DELETE FROM records", [])
        .expect_err("a foreign recording is read-only");
    assert!(err.to_string().contains("readonly") || err.to_string().contains("read-only"));
    assert_eq!(case.files(), ["ro.hephsession"]);
}

#[test]
fn a_zip_needs_exactly_one_session_entry() {
    let db = fixture();
    let case = Case::new();
    let two = zip_of(
        &[("b.hephsession", &db), ("a.hephsession", &db)],
        zip::CompressionMethod::Stored,
    );
    let why = not_a_session_why(case.open(&two));
    assert!(
        why.contains("2 *.hephsession entries")
            && why.contains(r#""a.hephsession", "b.hephsession""#),
        "{why}"
    );
    // The next step, by name.
    assert!(why.contains("unzip it and open one by path"), "{why}");

    let none = zip_of(
        &[("notes.txt", b"x"), ("dir.hephsession/", b"")],
        zip::CompressionMethod::Stored,
    );
    let why = not_a_session_why(case.open(&none));
    assert!(
        why.contains("no *.hephsession entry") && why.contains("notes.txt"),
        "{why}"
    );
}

/// A zip entry named `../x.hephsession` is read by content: nothing is
/// written to its name, inside or outside the case directory.
#[test]
fn a_zip_entry_name_is_never_a_path() {
    let case = Case::new();
    let inner = case.dir.path().join("inner");
    std::fs::create_dir_all(&inner).expect("mkdir");
    let zip = zip_of(
        &[("../x.hephsession", &zstd_of(&fixture()))],
        zip::CompressionMethod::Deflated,
    );
    let path = inner.join("artifact.zip");
    std::fs::write(&path, zip).expect("write zip");
    let opened = open(&path, &inner.join("tmp")).expect("open by content");
    assert!(!session_id(&opened).is_empty());
    drop(opened);
    assert_eq!(case.files(), ["inner/artifact.zip"]);
}

/// Decompression stops at the cap, refuses the file, and removes what it
/// wrote. The cap is a parameter: the real one is 4 GiB.
#[test]
fn decompression_stops_at_the_cap() {
    let case = Case::new();
    let big = zstd_of(&vec![0u8; 3 << 20]);
    let path = case.write("bomb.hephsession", &big);
    let why = not_a_session_why(open_capped(&path, &case.tmp(), 1 << 20));
    assert!(why.contains("decompresses past"), "{why}");
    assert_eq!(case.files(), ["bomb.hephsession"]);

    // The same bomb inside a zip.
    let zip = zip_of(&[("x.hephsession", &big)], zip::CompressionMethod::Stored);
    let path = case.write("bomb.zip", &zip);
    let why = not_a_session_why(open_capped(&path, &case.tmp(), 1 << 20));
    assert!(why.contains("decompresses past"), "{why}");
}

#[test]
fn the_wrong_content_is_not_a_session() {
    let case = Case::new();
    let why = not_a_session_why(case.open(b""));
    assert!(why.contains("empty"), "{why}");

    let why = not_a_session_why(case.open(b"<!DOCTYPE html><html>sign in</html>"));
    assert!(
        why.contains("3c 21 44 4f") && why.contains("<!DOCTYP"),
        "{why}"
    );

    // A zstd frame of something that is not SQLite.
    let why = not_a_session_why(case.open(&zstd_of(b"hello, world, not sqlite at all")));
    assert!(why.contains("not a SQLite file"), "{why}");

    // A zstd of a zip is not unwrapped twice.
    let zip = zip_of(
        &[("a.hephsession", &fixture())],
        zip::CompressionMethod::Stored,
    );
    let why = not_a_session_why(case.open(&zstd_of(&zip)));
    assert!(why.contains("not a SQLite file"), "{why}");
}

/// A truncated download, of either shape, is refused naming why, and never
/// panics.
#[test]
fn a_truncated_file_is_refused() {
    let db = fixture();
    let case = Case::new();
    let z = zstd_of(&db);
    let why = not_a_session_why(case.open(z.get(..z.len() / 2).expect("half")));
    assert!(why.contains("zstd"), "{why}");

    let why = not_a_session_why(case.open(db.get(..db.len() / 2).expect("half")));
    assert!(why.contains("truncated"), "{why}");

    // Cut inside the header.
    let why = not_a_session_why(case.open(db.get(..50).expect("head")));
    assert!(why.contains("not a SQLite file"), "{why}");
}

/// SQLite files that are not heph recordings: no application id, heph's id
/// without a `session` table, and a format newer than this heph reads.
#[test]
fn a_sqlite_file_must_be_a_known_recording() {
    let case = Case::new();
    let make = |name: &str, sql: &str| {
        let path = case.dir.path().join(name);
        let conn = Connection::open(&path).expect("create");
        conn.execute_batch(sql).expect("schema");
        drop(conn);
        path
    };
    let plain = make("plain.db", "CREATE TABLE session (id TEXT);");
    let why = not_a_session_why(open(&plain, &case.tmp()));
    assert!(why.contains("not a heph recording"), "{why}");

    let no_table = make(
        "no_table.db",
        &format!(
            "CREATE TABLE t (x); PRAGMA application_id = {}; PRAGMA user_version = 1;",
            store::APPLICATION_ID
        ),
    );
    let why = not_a_session_why(open(&no_table, &case.tmp()));
    assert!(why.contains("no `session` table"), "{why}");

    let no_format = make(
        "no_format.db",
        &format!(
            "CREATE TABLE session (id TEXT); PRAGMA application_id = {};",
            store::APPLICATION_ID
        ),
    );
    let why = not_a_session_why(open(&no_format, &case.tmp()));
    assert!(why.contains("no format"), "{why}");

    let too_new = make(
        "too_new.db",
        &format!(
            "CREATE TABLE session (id TEXT); PRAGMA application_id = {}; PRAGMA user_version = 99;",
            store::APPLICATION_ID
        ),
    );
    match open(&too_new, &case.tmp()) {
        Err(e @ ForeignError::TooNew { found: 99 }) => {
            let msg = e.to_string();
            assert!(
                msg.contains("99") && msg.contains(&format!("({})", store::FORMAT)),
                "{msg}"
            );
        }
        other => panic!("expected too new, got {other:?}"),
    }
}

/// `open` on another thread, failing the test if it does not return within
/// five seconds (a FIFO with no writer blocks a plain open forever).
fn open_bounded(path: PathBuf, tmp: PathBuf) -> Option<Result<ForeignFile, ForeignError>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || drop(tx.send(open(&path, &tmp))));
    rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
}

/// Anything but a regular file (after symlinks) is not a session, refused
/// without blocking: a FIFO, a directory, `.` and `..`.
#[test]
fn a_non_regular_file_is_not_a_session() {
    let case = Case::new();
    let fifo = case.dir.path().join("fifo.hephsession");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstr");
    // SAFETY: `c` is a valid NUL-terminated path for the call.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
    let dir = case.dir.path().join("dir.hephsession");
    std::fs::create_dir(&dir).expect("mkdir");
    let link = case.dir.path().join("link.hephsession");
    std::os::unix::fs::symlink(&fifo, &link).expect("symlink");
    for path in [fifo, dir, link, PathBuf::from("."), PathBuf::from("..")] {
        let opened = open_bounded(path.clone(), case.tmp()).expect("opening it hung");
        let why = not_a_session_why(opened);
        assert!(
            why.contains("not a regular file"),
            "{}: {why}",
            path.display()
        );
    }
}

/// A missing file is an IO error, not "not a session". Its message leaves
/// the path to the caller, which names it once.
#[test]
fn a_missing_file_is_an_io_error() {
    let case = Case::new();
    let path = case.dir.path().join("nope.hephsession");
    match open(&path, &case.tmp()) {
        Err(ForeignError::Io(e)) => assert!(!format!("{e:#}").contains("nope.hephsession")),
        other => panic!("expected an IO error, got {other:?}"),
    }
}

/// A zip's central directory is bounded before it is held in memory: past
/// `ZIP_ENTRIES_CAP` entries, or past `ZIP_DIRECTORY_CAP` bytes of it (a few
/// entries with long names), the zip is refused, a local path's included.
#[test]
fn a_zip_directory_is_capped() {
    let case = Case::new();
    let many: Vec<(String, &[u8])> = (0..=ZIP_ENTRIES_CAP)
        .map(|i| (format!("f{i}"), &b""[..]))
        .collect();
    let refs: Vec<(&str, &[u8])> = many.iter().map(|(n, b)| (n.as_str(), *b)).collect();
    let why = not_a_session_why(case.open(&zip_of(&refs, zip::CompressionMethod::Stored)));
    assert!(why.contains("past the cap of"), "{why}");

    let long: Vec<(String, &[u8])> = (0..150)
        .map(|i| (format!("{i:0>60000}"), &b""[..]))
        .collect();
    let refs: Vec<(&str, &[u8])> = long.iter().map(|(n, b)| (n.as_str(), *b)).collect();
    let why = not_a_session_why(case.open(&zip_of(&refs, zip::CompressionMethod::Stored)));
    assert!(why.contains("central directory is past"), "{why}");
}

/// The offsets of every `sig` in `bytes`.
fn offsets_of(bytes: &[u8], sig: &[u8; 4]) -> Vec<usize> {
    bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == sig)
        .map(|(i, _)| i)
        .collect()
}

/// Little-endian `value` over `bytes[at..at + 4]`.
fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes
        .get_mut(at..at + 4)
        .expect("in the zip")
        .copy_from_slice(&value.to_le_bytes());
}

const LOCAL_HEADER: &[u8; 4] = b"PK\x03\x04";
const CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";

/// An encrypted entry cannot be read without a password heph never asks
/// for: refused, naming the entry, and nothing left behind. The flag is set
/// by hand (general purpose bit 0, in the local and the central header),
/// as an encrypting archiver would set it.
#[test]
fn an_encrypted_zip_entry_is_refused_by_name() {
    let case = Case::new();
    let mut zip = zip_of(
        &[("run-7.hephsession", &zstd_of(&fixture()))],
        zip::CompressionMethod::Stored,
    );
    for at in offsets_of(&zip, LOCAL_HEADER) {
        *zip.get_mut(at + 6).expect("flags") |= 1;
    }
    for at in offsets_of(&zip, CENTRAL_HEADER) {
        *zip.get_mut(at + 8).expect("flags") |= 1;
    }
    let path = case.write("artifact.zip", &zip);
    let why = not_a_session_why(open(&path, &case.tmp()));
    assert!(
        why.contains("\"run-7.hephsession\"") && why.contains("Password required"),
        "{why}"
    );
    assert_eq!(case.files(), ["artifact.zip"]);
}

/// A zip64 archive (what an archiver writes for a large entry, and some
/// write for every entry) opens like any other.
#[test]
fn a_zip64_archive_opens() {
    let case = Case::new();
    let mut w = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    w.start_file("x.hephsession", opts).expect("start entry");
    w.write_all(&zstd_of(&fixture())).expect("write entry");
    let zip = w.finish().expect("finish zip").into_inner();
    // The zip64 extra field (header id 0x0001) is there.
    assert!(
        offsets_of(&zip, LOCAL_HEADER).iter().any(|at| {
            let name_len = zip.get(at + 26..at + 28).expect("name length");
            let name_len = usize::from(u16::from_le_bytes([name_len[0], name_len[1]]));
            zip.get(at + 30 + name_len..at + 32 + name_len) == Some(&[1, 0][..])
        }),
        "not a zip64 entry"
    );
    let path = case.write("artifact.zip", &zip);
    let opened = open(&path, &case.tmp()).expect("open a zip64 archive");
    assert!(!session_id(&opened).is_empty());
}

/// A zstd frame whose header declares a 2 GiB window is refused before
/// anything is decoded: the decoder would allocate the window. The frame is
/// written by hand: no content size, window descriptor exponent 21
/// (2^(10+21) bytes), then one raw last block.
#[test]
fn a_zstd_frame_wanting_a_2_gib_window_is_refused() {
    let case = Case::new();
    let data = b"SQLite format 3\0";
    let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 21 << 3];
    let block = u32::try_from(data.len() << 3).expect("small") | 1;
    frame.extend_from_slice(block.to_le_bytes().get(..3).expect("3 bytes"));
    frame.extend_from_slice(data);
    let why = not_a_session_why(case.open(&frame));
    assert!(why.contains("zstd") && why.contains("memory"), "{why}");
    assert_eq!(case.files(), ["in.hephsession"]);
}

/// A zip entry whose headers understate its size, stored or deflated, is
/// still stopped at the cap: the declared size is never trusted.
#[test]
fn an_understated_zip_entry_stops_at_the_cap() {
    let mut big = fixture();
    big.resize(3 << 20, 0);
    for method in [
        zip::CompressionMethod::Stored,
        zip::CompressionMethod::Deflated,
    ] {
        let case = Case::new();
        let mut zip = zip_of(&[("x.hephsession", &big)], method);
        // Uncompressed size: local header +22, central header +24.
        for at in offsets_of(&zip, LOCAL_HEADER) {
            put_u32(&mut zip, at + 22, 100);
        }
        for at in offsets_of(&zip, CENTRAL_HEADER) {
            put_u32(&mut zip, at + 24, 100);
        }
        let path = case.write("bomb.zip", &zip);
        let why = not_a_session_why(open_capped(&path, &case.tmp(), 1 << 20));
        // Stopped by the zip reader at the declared size, or by the cap;
        // either way refused, and nothing kept.
        assert!(
            why.contains("larger than its declared") || why.contains("decompresses past"),
            "{method:?}: {why}"
        );
        assert_eq!(case.files(), ["bomb.zip"], "{method:?}");
    }
}

/// A zip cut short (a download that stopped) is refused, naming the zip,
/// and never panics.
#[test]
fn a_truncated_zip_is_refused() {
    let zip = zip_of(
        &[("x.hephsession", &zstd_of(&fixture()))],
        zip::CompressionMethod::Deflated,
    );
    for cut in [zip.len() / 2, zip.len() - 10, 30] {
        let case = Case::new();
        let why = not_a_session_why(case.open(zip.get(..cut).expect("cut")));
        assert!(why.contains("cannot read the zip"), "cut at {cut}: {why}");
        assert_eq!(case.files(), ["in.hephsession"], "cut at {cut}");
    }
}
