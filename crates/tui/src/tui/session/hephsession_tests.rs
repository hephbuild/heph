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
    assert_eq!(session_id(&opened), session_id(&opened));
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

/// A missing file is an IO error naming it, not "not a session".
#[test]
fn a_missing_file_is_an_io_error() {
    let case = Case::new();
    let path = case.dir.path().join("nope.hephsession");
    match open(&path, &case.tmp()) {
        Err(ForeignError::Io(e)) => assert!(format!("{e:#}").contains("nope.hephsession")),
        other => panic!("expected an IO error, got {other:?}"),
    }
}
