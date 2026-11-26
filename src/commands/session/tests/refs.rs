//! `<ref>` resolution (D8) and the viewers over a foreign recording (I7):
//! the spec's "Adversarial inputs for `<ref>` and the sniffer" rows.

use std::io::Write as _;

use super::*;
use crate::tui::session::recording::Recording as Opened;
use crate::tui::session::{FileSource, store};

/// A download serving `body`, as a server would: into a temp file in `dir`.
pub(super) struct Serve(pub(super) Vec<u8>);

impl reference::Download for Serve {
    fn get_to_temp(&self, _url: &str, dir: &Path) -> anyhow::Result<tempfile::NamedTempFile> {
        let mut f = tempfile::NamedTempFile::new_in(dir).context("creating the temp file")?;
        f.write_all(&self.0).context("writing the body")?;
        Ok(f)
    }
}

/// A download that fails the way reqwest does: naming the URL it was given,
/// credentials and all.
struct FailNamingUrl;

impl reference::Download for FailNamingUrl {
    fn get_to_temp(&self, url: &str, _dir: &Path) -> anyhow::Result<tempfile::NamedTempFile> {
        anyhow::bail!("error sending request for url ({url})")
    }
}

pub(super) fn opener_in(
    tmp: &Path,
    download: impl reference::Download + 'static,
) -> reference::Opener {
    reference::Opener {
        tmp_dir: Arc::from(tmp),
        download: Arc::new(download),
    }
}

pub(super) async fn show_with(
    sources: &[&dyn SessionSource],
    arg: &str,
    json: bool,
    opener: &reference::Opener,
) -> anyhow::Result<String> {
    let mut out = Vec::new();
    show::show(
        sources,
        opts(),
        arg,
        &json_args(json),
        piped(&mut out),
        opener,
    )
    .await?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

/// `attach` on the line backend, and everything it printed through its sink.
pub(super) async fn attach_captured(
    sources: &[&dyn SessionSource],
    arg: &str,
    opener: &reference::Opener,
) -> (anyhow::Result<()>, String) {
    let slot = Arc::new(crate::tui::session::SessionSlot::new());
    let sink = crate::tui::LogSink::new_direct_with(Arc::clone(&slot));
    // The line backend never switches the sink back: every line lands here.
    let mut rx = sink.switch_to_buffered();
    // Boxed: `attach`'s future is over clippy's `large_futures` threshold.
    let res = Box::pin(attach::attach(
        sources,
        opts(),
        arg,
        sink,
        &slot,
        &crate::commands::GlobalOptions {
            no_tui: true,
            ..Default::default()
        },
        opener,
    ))
    .await;
    let mut printed = Vec::new();
    while let Ok(line) = rx.try_recv() {
        printed.extend(line.bytes);
    }
    (res, String::from_utf8_lossy(&printed).into_owned())
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// D8: the shape alone decides, with no filesystem probe.
#[test]
fn ref_classification() {
    use reference::{RefKind, classify};
    for (arg, want) in [
        ("02kf5", RefKind::Id("02kf5")),
        ("12345", RefKind::Id("12345")),
        ("02KF5ABCDE", RefKind::Id("02KF5ABCDE")),
        ("./02kf5", RefKind::Path(Path::new("./02kf5"))),
        (
            "02kf5.hephsession",
            RefKind::Path(Path::new("02kf5.hephsession")),
        ),
        ("a/b", RefKind::Path(Path::new("a/b"))),
        (
            "./run#2.hephsession",
            RefKind::Path(Path::new("./run#2.hephsession")),
        ),
        (
            "run?x.hephsession",
            RefKind::Path(Path::new("run?x.hephsession")),
        ),
        ("/abs/run", RefKind::Path(Path::new("/abs/run"))),
        ("https://h/x", RefKind::Url("https://h/x")),
        ("HTTPS://h/x", RefKind::Url("HTTPS://h/x")),
        ("hTtPs://h/x", RefKind::Url("hTtPs://h/x")),
    ] {
        assert_eq!(classify(arg).expect(arg), want, "{arg}");
    }
    for arg in [
        "http://h/x.hephsession",
        "HTTP://h/x",
        "http://u:hunter2@h/x",
    ] {
        let e = classify(arg).expect_err(arg);
        assert_eq!(error_kind(&e), ErrorKind::Fetch, "{arg}");
        let msg = format!("{e:#}");
        assert!(msg.contains("https://"), "{msg}");
        assert!(!msg.contains("hunter2"), "{msg}");
        // The next step, by name.
        assert!(
            msg.contains("use https://, or download the file and open it by path"),
            "{msg}"
        );
    }
}

/// The opener `execute` builds downloads `https://` only, capped at
/// `URL_CAP`: a `<ref>` URL is untrusted. An `http://` URL is refused by the
/// download itself, before any connection, as well as by `classify`.
#[test]
fn the_production_opener_is_https_only_and_capped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let opener = reference::Opener::new(tmp.path().to_path_buf());
    assert_eq!(&*opener.tmp_dir, tmp.path());
    let fetcher = opener.download.fetcher().expect("an hfetch::Fetcher");
    assert!(fetcher.https_only, "{fetcher:?}");
    assert_eq!(
        fetcher.max_bytes,
        Some(crate::tui::session::hephsession::URL_CAP),
        "{fetcher:?}"
    );
    let e = opener
        .download
        .get_to_temp("http://127.0.0.1:1/x.hephsession", tmp.path())
        .expect_err("http is refused");
    assert!(e.downcast_ref::<hfetch::NotHttps>().is_some(), "{e:#}");
}

/// O4: a URL with credentials is refused before anything is fetched, and the
/// credentials never reach the error text.
#[tokio::test]
async fn credentials_in_a_url_are_refused_and_never_echoed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    let op = opener_in(tmp.path(), FailNamingUrl);
    for url in [
        "https://u:hunter2@h.invalid/x.hephsession",
        "https://hunter2@h.invalid/x.hephsession",
        "HTTPS://u:hunter2@h.invalid/x",
    ] {
        let e = show_with(&sources, url, false, &op).await.expect_err(url);
        assert_eq!(error_kind(&e), ErrorKind::Fetch, "{url}");
        let mut out = Vec::new();
        write_json_error(&e, &mut out).expect("json");
        for text in [
            format!("{e:#}"),
            format!("{e:?}"),
            String::from_utf8(out).expect("utf-8"),
        ] {
            assert!(!text.contains("hunter2"), "{url}: {text}");
        }
    }
}

/// A signed URL's query and fragment are a credential: they never reach
/// `foreign.origin`, the "read from" or "replaying" lines, or an error's
/// `--json` message. What the download itself reports is `hfetch`'s to
/// redact (its own test).
#[tokio::test]
async fn a_signed_url_query_never_reaches_the_output() {
    const BASE: &str = "https://bucket.invalid/run.hephsession";
    const SIGNED: &str =
        "https://bucket.invalid/run.hephsession?X-Amz-Signature=sekrit&X-Amz-Credential=AKIA#frag";
    let clean = |text: &str| {
        for s in ["sekrit", "AKIA", "frag", "X-Amz"] {
            assert!(!text.contains(s), "{s} in: {text}");
        }
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(T0, 12);
    let dir = write_ended(
        &tmp.path().join("sessions"),
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, None),
        &ended(Some(0), T0 + 10),
    );
    let body = std::fs::read(dir.join(store::FILE)).expect("read the recording");
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    let ftmp = tempfile::tempdir().expect("tempdir");
    let op = opener_in(ftmp.path(), Serve(body));

    let text = show_with(&sources, SIGNED, false, &op)
        .await
        .expect("show the download");
    clean(&text);
    assert_eq!(
        text.lines().next(),
        Some(format!("read from {BASE}").as_str()),
        "{text}"
    );
    let v = json(
        &show_with(&sources, SIGNED, true, &op)
            .await
            .expect("show --json"),
    );
    clean(&v.to_string());
    assert_eq!(v["foreign"]["origin"], BASE, "{v}");
    // No End record among these events: attach replays, then says so.
    let (res, printed) = attach_captured(&sources, SIGNED, &op).await;
    clean(&format!("{:#}", res.expect_err("no recorded end")));
    clean(&printed);
    assert!(
        printed.starts_with(&format!("heph: replaying {BASE}\n")),
        "{printed}"
    );

    // An error names the URL the same way.
    let html = opener_in(ftmp.path(), Serve(b"<!doctype html>".to_vec()));
    for url in [
        SIGNED,
        "http://bucket.invalid/run?X-Amz-Signature=sekrit#frag",
    ] {
        let e = show_with(&sources, url, true, &html)
            .await
            .expect_err("not a session");
        let mut out = Vec::new();
        write_json_error(&e, &mut out).expect("json");
        let out = String::from_utf8(out).expect("utf-8");
        clean(&out);
        clean(&format!("{e:#}"));
        assert!(
            json(&out)["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("bucket.invalid/run")),
            "{out}"
        );
    }
}

/// A path that cannot be read is `io`, and its message names it once.
#[tokio::test]
async fn an_unreadable_path_is_io_and_named_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    let op = opener_in(tmp.path(), NoDownload);
    let arg = tmp.path().join("nope.hephsession").display().to_string();
    let e = show_with(&sources, &arg, false, &op)
        .await
        .expect_err("missing");
    assert_eq!(error_kind(&e), ErrorKind::Io);
    let msg = format!("{e:#}");
    assert_eq!(msg.matches("nope.hephsession").count(), 1, "{msg}");
}

/// A local recording's `OpenError` keeps its kind through the context the
/// command adds: `error.kind` reads it, not `other`.
#[test]
fn an_open_error_keeps_its_kind() {
    use crate::tui::session::store::OpenError;
    for (e, kind) in [
        (OpenError::Missing, ErrorKind::NotFound),
        (OpenError::NotASession, ErrorKind::NotASession),
        (OpenError::TooNew { found: 99 }, ErrorKind::FormatTooNew),
        (OpenError::Other(anyhow::anyhow!("disk")), ErrorKind::Other),
    ] {
        let e = anyhow::Error::new(e).context("opening session x");
        assert_eq!(error_kind(&e), kind, "{e:#}");
    }
}

/// The spec's `12345`: a live session's pid first, then an id that starts or
/// ends with it, then an error naming both readings.
#[tokio::test]
async fn digits_are_a_live_pid_then_part_of_an_id() {
    let by_pid = SessionId::new(T0, 1);
    let by_prefix = SessionId::parse("12345VWXYZ").expect("an id");
    let other = SessionId::new(T0 + 5_000, 3);
    let session = |id, pid, live| {
        (
            entry(meta(id, pid, &["heph", "run"]), live, None),
            Vec::new(),
        )
    };
    let resolved = |source: FakeSource| async move {
        let sources: [&dyn SessionSource; 1] = [&source];
        resolve(&sources, "12345", opts()).await.map(|(_, e)| e.id)
    };

    let both = FakeSource {
        sessions: vec![session(by_prefix, 7, false), session(by_pid, 12345, true)],
    };
    assert_eq!(resolved(both).await.expect("the pid"), by_pid);
    // A pid that is not live is not one: the id is.
    let dead = FakeSource {
        sessions: vec![session(by_prefix, 7, false), session(other, 12345, false)],
    };
    assert_eq!(resolved(dead).await.expect("the prefix"), by_prefix);
    let neither = FakeSource {
        sessions: vec![session(other, 12345, false)],
    };
    let e = resolved(neither).await.expect_err("no match");
    assert_eq!(error_kind(&e), ErrorKind::NotFound);
    let msg = format!("{e:#}");
    assert!(
        msg.contains("no live session has pid 12345")
            && msg.contains("no session id starts or ends with 12345")
            && msg.contains("`heph session list --all` lists them"),
        "{msg}"
    );
}

/// https serving a web page: not a session, 125, and under `--json` the
/// typed error object.
#[tokio::test]
async fn a_url_serving_html_is_not_a_session() {
    const URL: &str = "https://example.invalid/run.hephsession";
    let tmp = tempfile::tempdir().expect("tempdir");
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    let op = opener_in(
        tmp.path(),
        Serve(b"<!doctype html><html><body>Sign in</body></html>".to_vec()),
    );
    let e = show_with(&sources, URL, true, &op).await.expect_err("html");
    assert_eq!(error_kind(&e), ErrorKind::NotASession);
    let mut out = Vec::new();
    write_json_error(&e, &mut out).expect("json");
    let out = String::from_utf8(out).expect("utf-8");
    assert_eq!(out.lines().count(), 1, "{out}");
    let v = json(&out);
    assert_eq!(v["error"]["kind"], "not_a_session", "{out}");
    let msg = v["error"]["message"].as_str().expect("message");
    assert!(msg.contains(URL), "{msg}");
    assert_eq!(exit_code(&wrap_exit(Err(e)).expect_err("err")), Some(125));
    // The download went with the error.
    assert_eq!(
        std::fs::read_dir(tmp.path()).expect("read_dir").count(),
        0,
        "nothing left in the temp dir"
    );
}

/// An empty `<ref>` names nothing; a heph recording in a newer format says
/// so by kind, and still exits 125.
#[tokio::test]
async fn an_empty_or_too_new_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    let op = opener_in(tmp.path(), NoDownload);
    for arg in ["", "  "] {
        let e = show_with(&sources, arg, false, &op)
            .await
            .expect_err("empty");
        assert_eq!(error_kind(&e), ErrorKind::NotFound, "{arg:?}");
        assert_eq!(exit_code(&wrap_exit(Err(e)).expect_err("err")), Some(125));
    }

    let newer = tmp.path().join("newer.hephsession");
    rusqlite::Connection::open(&newer)
        .expect("create")
        .execute_batch(&format!(
            "CREATE TABLE session (id TEXT); PRAGMA application_id = {}; PRAGMA user_version = 99;",
            store::APPLICATION_ID
        ))
        .expect("write a newer format");
    let arg = newer.display().to_string();
    let e = show_with(&sources, &arg, false, &op)
        .await
        .expect_err("too new");
    assert_eq!(error_kind(&e), ErrorKind::FormatTooNew);
    let msg = format!("{e:#}");
    // Both formats, and what to do.
    assert!(
        msg.contains(&arg)
            && msg.contains("format 99")
            && msg.contains(&format!("({})", store::FORMAT))
            && msg.contains("upgrade heph"),
        "{msg}"
    );
    let mut out = Vec::new();
    write_json_error(&e, &mut out).expect("json");
    assert_eq!(
        json(std::str::from_utf8(&out).expect("utf-8"))["error"]["kind"],
        "format_too_new"
    );
    assert_eq!(exit_code(&wrap_exit(Err(e)).expect_err("err")), Some(125));
}

/// I7: a path is read from itself alone. The same session, live in the
/// sessions directory under a held lock, is not probed, listed or read:
/// opened by path it is incomplete, never running, has no in-flight report,
/// and attach says it has no recorded end.
#[tokio::test]
async fn a_path_never_reads_the_local_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = SessionId::new(T0, 9);
    let dir = write_ended(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, None),
        &ended(Some(0), T0 + 10),
    );
    // Unsealed, in WAL mode and locked: a running owner, read locally.
    testing::alter(
        &dir,
        "UPDATE session SET sealed = 0, exit_code = NULL, ended_ms = NULL; DELETE FROM status;
         PRAGMA journal_mode = WAL;",
    )
    .expect("unseal");
    let _lock = hold_lock(&dir);
    let file = FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let local = json(
        &show_with(&sources, &id.encode(), true, &no_opener())
            .await
            .expect("show by id"),
    );
    assert_eq!(local["state"], "running", "{local}");
    assert!(local.get("foreign").is_none(), "{local}");

    let path = dir.join(store::FILE);
    let arg = path.display().to_string();
    let ftmp = tempfile::tempdir().expect("tempdir");
    let op = opener_in(ftmp.path(), NoDownload);
    let text = show_with(&sources, &arg, false, &op)
        .await
        .expect("show by path");
    assert_eq!(
        text.lines().next(),
        Some(format!("read from {arg}: incomplete").as_str()),
        "{text}"
    );
    let v = json(
        &show_with(&sources, &arg, true, &op)
            .await
            .expect("show by path"),
    );
    assert_eq!(v["state"], "incomplete", "{v}");
    assert_eq!(v["foreign"]["partial"], "incomplete", "{v}");
    assert_eq!(v["foreign"]["origin"], arg.as_str(), "{v}");

    let foreign = Opened::open_foreign(&path, ftmp.path())
        .expect("open")
        .into_source()
        .expect("source");
    let e = foreign
        .inflight(id, Duration::from_secs(1))
        .await
        .expect_err("no in-flight report for a file");
    assert!(format!("{e:#}").contains(&arg), "{e:#}");

    let (res, printed) = attach_captured(&sources, &arg, &op).await;
    let e = res.expect_err("no end");
    assert_eq!(exit_code(&e), Some(125));
    let msg = format!("{e:#}");
    assert!(
        msg.contains(&format!("{arg} has no recorded end (incomplete)")),
        "{msg}"
    );
    assert!(
        printed.starts_with(&format!("heph: replaying {arg}: incomplete\n")),
        "{printed}"
    );
}

/// A file whose name holds `#` or `?` (a URI's fragment and query markers)
/// opens by that name: SQLite is handed a `file:` URI, and an unescaped `#`
/// would open `run` instead, or nothing.
#[tokio::test]
async fn a_path_with_uri_characters_opens() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = SessionId::new(T0, 11);
    let dir = write_ended(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        &ended(Some(0), T0 + 10),
    );
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    for name in [
        "run#2.hephsession",
        "run?x.hephsession",
        "run%23.hephsession",
    ] {
        let out = tempfile::tempdir().expect("tempdir");
        let path = out.path().join(name);
        std::fs::copy(dir.join(store::FILE), &path).expect("copy the recording");
        let arg = path.display().to_string();
        let ftmp = tempfile::tempdir().expect("tempdir");
        let op = opener_in(ftmp.path(), NoDownload);
        let v = json(
            &show_with(&sources, &arg, true, &op)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e:#}")),
        );
        assert_eq!(v["id"], id.encode().as_str(), "{name}: {v}");
        assert_eq!(v["foreign"]["origin"], arg.as_str(), "{name}: {v}");
    }
}

/// Recorded text from a file, in a Log record, a notice and a failure log,
/// never reaches the terminal raw: no ESC, no bidi control, no NUL.
#[tokio::test]
async fn foreign_text_never_reaches_the_terminal_raw() {
    const RAW: &str = "clear\u{1b}[2J flip\u{202e}gnp.exe nul\u{0}end";
    const ESCAPED: &str = r"clear\u{1b}[2J flip\u{202e}gnp.exe nul\u{0}end";
    let hex: String = RAW.bytes().map(|b| format!("{b:02x}")).collect();
    let tmp = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(T0, 4);
    let mut records = build_records("//a:b", T0, Some(1));
    records.insert(
        0,
        SessionRecord::session(
            T0,
            SessionRecordKind::Log {
                level: "INFO".to_owned(),
                msg: RAW.to_owned(),
                seq: 1,
            },
        ),
    );
    let dir = write_ended(
        &tmp.path().join("elsewhere"),
        &meta(id, 1, &["heph", "run"]),
        &records,
        &ended(Some(1), T0 + 1_000),
    );
    testing::alter(
        &dir,
        &format!(
            "INSERT INTO notices VALUES ('//a:gate', 1, 'plan', CAST(x'{hex}' AS TEXT));
             INSERT INTO failure_logs VALUES ('//a:b', 0, x'{hex}', 0);"
        ),
    )
    .expect("add the notice and the log");
    let arg = dir.join(store::FILE).display().to_string();
    let ftmp = tempfile::tempdir().expect("tempdir");
    let op = opener_in(ftmp.path(), NoDownload);
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];

    let raw = |s: &str| s.contains(['\u{1b}', '\u{202e}', '\u{0}']);
    let text = show_with(&sources, &arg, false, &op).await.expect("show");
    assert!(!raw(&text), "{text:?}");
    assert!(
        text.contains("approval notice plan of request 1 for //a:gate:")
            && text.contains("log of //a:b:"),
        "{text}"
    );
    // Both blocks, escaped.
    assert_eq!(text.matches(ESCAPED).count(), 2, "{text}");

    let (res, printed) = attach_captured(&sources, &arg, &op).await;
    assert_eq!(exit_code(&res.expect_err("exit 1")), Some(1));
    assert!(!raw(&printed), "{printed:?}");
    assert!(printed.contains(ESCAPED), "{printed}");
}

/// I7 for a snapshot: exported while its session ran, every local trace of
/// it removed, the export still says what it is, and attach's dashboard
/// shows the approval notice it carried, not what this machine would read.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_export_reads_only_itself() {
    use crate::tui::session::{ApprovalPrompt, ApprovalPromptNotice, ApprovalQueued};
    use crate::tui::{App as _, TUIAppView as _};

    const NOTICE: &str = "drop the table";
    let ws = tempfile::tempdir().expect("tempdir");
    std::fs::write(ws.path().join(".hephconfig"), "").expect("write .hephconfig");
    let global = crate::commands::GlobalOptions::default();
    let (engine, home) =
        crate::commands::bootstrap::new_reader_engine(&global, ws.path()).expect("reader engine");
    drop(engine);
    std::fs::write(ws.path().join("PLAN"), NOTICE).expect("write the notice source");
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(NOTICE.as_bytes());
    hasher.update(&[0]);
    let notice = ApprovalPromptNotice {
        name: "plan".to_owned(),
        sources: vec![hcore::approval_notice::NoticeSource {
            addr: "//a:plan".to_owned(),
            origin: hcore::approval_notice::NoticeOrigin::File {
                path: "PLAN".to_owned(),
                out_path: "PLAN".to_owned(),
                x: false,
                hashout: format!("{:x}", hasher.digest()),
            },
        }],
    };
    let sessions = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(T0, 8);
    let mut m = meta(id, 1, &["heph", "run"]);
    m.interactive = true;
    m.workspace_root = Some(ws.path().display().to_string());
    m.home_dir = Some(home.display().to_string());
    let records = [SessionRecord::session(
        T0,
        SessionRecordKind::Approval {
            prompt: Some(Box::new(ApprovalPrompt {
                queue: vec![ApprovalQueued {
                    seq: 1,
                    addr: "//a:gate".to_owned(),
                    queued_at_unix_ms: T0,
                    notices: vec![notice],
                }],
            })),
            standing: None,
        },
    )];
    let live = write_live(sessions.path(), &m, &records, None);
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("snap.hephsession");
    let exported = {
        let file = FileSource::new(sessions.path());
        let mut stdout = Vec::new();
        export::export_ref(
            &file,
            opts(),
            &ExportArgs {
                reference: id.encode(),
                output: Some(path.clone()),
                no_logs: false,
            },
            crate::engine::approval::read_notice_from_store,
            &mut stdout,
            &no_opener(),
        )
        .await
        .expect("export")
    };
    assert!(exported.snapshot);
    drop((live, sessions, ws));
    assert!(!home.exists(), "the home was under the workspace");

    let arg = path.display().to_string();
    let ftmp = tempfile::tempdir().expect("tempdir");
    let op = opener_in(ftmp.path(), NoDownload);
    let empty = FakeSource::default();
    let sources: [&dyn SessionSource; 1] = [&empty];
    let text = show_with(&sources, &arg, false, &op).await.expect("show");
    assert_eq!(
        text.lines().next(),
        Some(format!("read from {arg}: snapshot of a running session").as_str()),
        "{text}"
    );
    let v = json(&show_with(&sources, &arg, true, &op).await.expect("show"));
    // Its own state: neither `lost` nor `running`, which say something about
    // a process this viewer cannot see.
    assert_eq!(v["state"], "snapshot", "{v}");
    assert_eq!(v["foreign"]["partial"], "snapshot", "{v}");
    assert_eq!(v["foreign"]["notices"][0]["text"], NOTICE, "{v}");

    // The dashboard reads the notice from the export: the home it was read
    // from is gone.
    let foreign = Opened::open_foreign(&path, ftmp.path())
        .expect("open")
        .into_source()
        .expect("source");
    let (_detach_tx, detach_rx) = tokio::sync::mpsc::unbounded_channel();
    let app = attach::AttachApp::new(
        &global,
        m,
        attach::Origin::Foreign(&foreign),
        foreign.holder_names(id),
        futures::stream::empty().boxed(),
        detach_rx,
        true,
    );
    let mut follow = foreign.subscribe(id).await.expect("subscribe");
    while let Some(item) = follow.next().await {
        match item.expect("item") {
            SessionItem::Record(r) => drop(app.feed().route(*r)),
            SessionItem::CaughtUp { .. } => break,
            SessionItem::Ended { .. } => panic!("caught up first"),
        }
    }
    let mut view = app.tui_view();
    assert!(view.approval_open(), "a pending request opens");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame: String = view
            .render_full(NOW, 120, 40)
            .iter()
            .map(|l| {
                let mut row: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                row.push('\n');
                row
            })
            .collect();
        if frame.contains(NOTICE) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the recorded notice never showed:\n{frame}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (res, printed) = attach_captured(&sources, &arg, &op).await;
    let e = res.expect_err("no end");
    assert_eq!(exit_code(&e), Some(125));
    let msg = format!("{e:#}");
    assert!(
        msg.contains(&format!(
            "{arg} has no recorded end (snapshot of a running session)"
        )),
        "{msg}"
    );
    assert!(printed.starts_with("heph: replaying "), "{printed}");
}

/// A snapshot exported while its run waited on a lock: read elsewhere it is
/// no live stall. The text draws no "Stuck on" block (that block is for a
/// run this viewer watches), and `--json` keeps the wait in `blocked_on` as
/// the recording left it, with no holder session: none of this machine's
/// sessions is the one that held the lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_snapshot_is_no_live_stall() {
    let sessions = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(T0, 13);
    let m = meta(id, 1, &["heph", "run", "//go/..."]);
    let mut records = build_records("//go:sdk", T0, None);
    records.push(record(
        T0 + 100,
        BuildEventKind::ResultLockWaitStart {
            addr: "//go:sdk".into(),
            holder_pid: Some(300),
            in_use_by_readers: false,
        },
    ));
    let live = write_live(sessions.path(), &m, &records, None);
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("blocked.hephsession");
    let exported = export::export_ref(
        &FileSource::new(sessions.path()),
        opts(),
        &ExportArgs {
            reference: id.encode(),
            output: Some(path.clone()),
            no_logs: true,
        },
        crate::engine::approval::read_notice_from_store,
        &mut Vec::new(),
        &no_opener(),
    )
    .await
    .expect("export");
    assert!(exported.snapshot);
    drop((live, sessions));

    let arg = path.display().to_string();
    let ftmp = tempfile::tempdir().expect("tempdir");
    let op = opener_in(ftmp.path(), NoDownload);
    // A local session held by pid 300: a foreign wait never points at it.
    let holder = SessionId::new(T0 + 9_000, 3);
    let here = FakeSource {
        sessions: vec![(
            entry(meta(holder, 300, &["heph", "run", "//..."]), true, None),
            Vec::new(),
        )],
    };
    let sources: [&dyn SessionSource; 1] = [&here];
    let text = show_with(&sources, &arg, false, &op).await.expect("show");
    assert!(!text.contains("Stuck on"), "{text}");
    assert!(!text.contains(&holder.encode()), "{text}");
    let v = json(&show_with(&sources, &arg, true, &op).await.expect("show"));
    assert_eq!(v["state"], "snapshot", "{v}");
    assert_eq!(v["chain_root"], serde_json::Value::Null, "{v}");
    assert_eq!(
        v["blocked_on"],
        serde_json::json!([{"kind": "lock", "addr": "//go:sdk", "holder_pid": 300, "holder_session": null}]),
        "{v}"
    );
}
