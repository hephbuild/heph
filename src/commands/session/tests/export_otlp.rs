//! `heph session export --format otlp`: the command around
//! `tui::session::otlp` (whose mapping its own tests hold). What is tested
//! here is the command's: which recording it reads, where the lines go, and
//! what it refuses.

use super::refs::opener_in;
use super::*;
use crate::tui::session::FileSource;

fn otlp_args(reference: String, output: &Path, no_logs: bool) -> ExportArgs {
    ExportArgs {
        reference,
        output: Some(output.to_path_buf()),
        no_logs,
        format: ExportFormat::Otlp,
    }
}

async fn run_export(
    file: &FileSource,
    args: &ExportArgs,
    opener: &reference::Opener,
) -> (anyhow::Result<export::Exported>, Vec<u8>) {
    let mut stdout = Vec::new();
    let res = export::export_ref(
        file,
        opts(),
        args,
        crate::engine::approval::read_notice_from_store,
        &mut stdout,
        opener,
        no_note(),
    )
    .await;
    (res, stdout)
}

/// Each line one JSON object: traces first, the session's id on its
/// resource, and at least one span.
fn assert_otlp(text: &str, id: SessionId) {
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();
    let first = lines.first().expect("a traces line");
    let rs = &first["resourceSpans"][0];
    let attrs = rs["resource"]["attributes"]
        .as_array()
        .expect("resource attributes");
    let session_id = attrs
        .iter()
        .find(|a| a["key"] == "heph.session.id")
        .expect("heph.session.id");
    assert_eq!(
        session_id["value"]["stringValue"],
        id.encode().as_str(),
        "{text}"
    );
    assert!(
        rs["scopeSpans"][0]["spans"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "{text}"
    );
}

/// `export --format otlp` of a local recording to `-o PATH` and to stdout,
/// and of the same session exported as a `.hephsession` and named by path:
/// the three write the same lines. A file export leaves no temp file
/// behind; `--no-logs` is refused and writes nothing; a running session's
/// export says it is a snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn export_otlp_local_foreign_stdout_and_file() {
    let sessions = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(T0, 21);
    write_ended(
        sessions.path(),
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &build_records("//a:b", T0, Some(0)),
        &ended(Some(0), T0 + 10),
    );
    let file = FileSource::new(sessions.path());
    let out = tempfile::tempdir().expect("tempdir");

    // -o PATH.
    let path = out.path().join("run.otlp.jsonl");
    let (res, stdout) =
        run_export(&file, &otlp_args(id.encode(), &path, false), &no_opener()).await;
    let exported = res.expect("export to a file");
    assert_eq!(exported.path.as_deref(), Some(path.as_path()));
    assert!(!exported.snapshot);
    assert!(stdout.is_empty(), "a file export writes nothing to stdout");
    let local = std::fs::read_to_string(&path).expect("read the export");
    assert_otlp(&local, id);
    let names: Vec<_> = std::fs::read_dir(out.path())
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["run.otlp.jsonl"], "no temp file is left behind");

    // -o -: the same lines on stdout.
    let (res, piped) = run_export(
        &file,
        &otlp_args(id.encode(), Path::new("-"), false),
        &no_opener(),
    )
    .await;
    assert!(res.expect("export to stdout").path.is_none());
    assert_eq!(String::from_utf8(piped).expect("utf-8"), local);

    // A foreign recording, named by path: the session exported as a
    // `.hephsession` first.
    let hs = out.path().join("run.hephsession");
    let (res, _) = run_export(
        &file,
        &ExportArgs {
            format: ExportFormat::Hephsession,
            ..otlp_args(id.encode(), &hs, false)
        },
        &no_opener(),
    )
    .await;
    res.expect("export a .hephsession");
    let ftmp = tempfile::tempdir().expect("tempdir");
    let op = opener_in(ftmp.path(), NoDownload);
    let (res, foreign) = run_export(
        &file,
        &otlp_args(hs.display().to_string(), Path::new("-"), false),
        &op,
    )
    .await;
    assert!(!res.expect("export a foreign recording").snapshot);
    assert_eq!(String::from_utf8(foreign).expect("utf-8"), local);

    // --no-logs: refused, nothing written.
    let refused = out.path().join("refused.otlp.jsonl");
    let (res, stdout) =
        run_export(&file, &otlp_args(id.encode(), &refused, true), &no_opener()).await;
    let e = res.expect_err("--no-logs is refused");
    assert!(format!("{e:#}").contains("--no-logs"), "{e:#}");
    assert!(!refused.exists());
    assert!(stdout.is_empty());

    // A running session, one result in: a snapshot, holding that result.
    let live_id = SessionId::new(T0, 22);
    let mut records = build_records("//a:c", T0, None);
    records.push(record(
        T0 + 40,
        BuildEventKind::ResultEnd {
            spans: Vec::new(),
            left_behind: None,
            cancelled: false,
            addr: "//a:c".into(),
            error: None,
            upstream_of: None,
            exit_status: None,
            exit_code: None,
            log_tail: None,
            timed_out: None,
        },
    ));
    let live = write_live(
        sessions.path(),
        &meta(live_id, 2, &["heph", "run", "//a:c"]),
        &records,
        None,
    );
    let (res, piped) = run_export(
        &file,
        &otlp_args(live_id.encode(), Path::new("-"), false),
        &no_opener(),
    )
    .await;
    assert!(res.expect("export a running session").snapshot);
    assert_otlp(&String::from_utf8(piped).expect("utf-8"), live_id);
    drop(live);
}
