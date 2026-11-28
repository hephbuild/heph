//! I7: nothing heph records or prints for a failure holds a secret. One
//! request fails three ways with secrets planted in each: a fetch whose
//! `${read://}` URL carries a password, a query and a fragment; a run handed
//! a credential and a `${read://}` value that prints both before it exits; a
//! credential whose material is not the shape its field mapping asks for.
//! Each output a failure reaches is then searched for every secret: stderr
//! (the CI box and the interactive report), `run --json`, `inspect failure
//! --json` and the slot's `diagnostic.json`.
#![expect(
    clippy::panic_in_result_fn,
    clippy::let_underscore_must_use,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use crate::run_json::{JsonRun, run_json};
use heph::commands::inspect::failure::{FailureView, inspect_view, print_to};
use heph::engine::Engine;
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use std::io::{Read as _, Write as _};

/// The `${read://}` URL's password, query and fragment.
const URL_SECRETS: [&str; 3] = [
    "DEFERRED-USERINFO-SECRET-1",
    "DEFERRED-QUERY-SECRET-2",
    "DEFERRED-FRAGMENT-SECRET-3",
];
/// A `${read://}` value the run prints.
const READ_SECRET: &str = "DEFERRED-ENV-SECRET-4";
/// A credential's material the run prints.
const CRED_SECRET: &str = "CRED-MATERIAL-SECRET-5";
/// Material at a field the mapping wants a string at, but is an object.
const SUBTREE_SECRET: &str = "CRED-SUBTREE-SECRET-6";

/// Every planted secret.
fn secrets() -> impl Iterator<Item = &'static str> {
    URL_SECRETS
        .into_iter()
        .chain([READ_SECRET, CRED_SECRET, SUBTREE_SECRET])
}

/// The three culprits.
const CULPRITS: [&str; 3] = ["//app:fetch", "//app:cred", "//app:badcred"];

/// A loopback server that answers every request 404.
fn serve_404() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let host = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    host
}

/// The workspace: `//app:all` depends on the three culprits.
fn workspace() -> Workspace {
    let host = serve_404();
    let [pw, query, fragment] = URL_SECRETS;
    let ws = Workspace::new();
    ws.write_build_file(
        "val",
        &format!(
            r#"
target(name = "url", driver = "bash", out = "url.txt",
       run = "printf '%s' 'http://heph-user:{pw}@{host}/asset?sig={query}#{fragment}' > url.txt")
target(name = "plain", driver = "bash", out = "v.txt",
       run = "printf '%s' '{READ_SECRET}' > v.txt")
"#
        ),
    );
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(name = "t", driver = "auth.credential",
       sources = [heph.auth.exec(["sh", "-c", "printf '{{\"token\":\"{CRED_SECRET}\"}}'"],
           fields = {{"token": "token"}})],
       present = {{"env": {{"TOKEN": "${{token}}"}}}})
target(name = "bad", driver = "auth.credential",
       sources = [heph.auth.exec(["sh", "-c", "printf '{{\"token\":{{\"inner\":\"{SUBTREE_SECRET}\"}}}}'"],
           fields = {{"token": "token"}})],
       present = {{"env": {{"TOKEN": "${{token}}"}}}})
"#
        ),
    );
    ws.write_build_file(
        "app",
        r#"
target(name = "fetch", driver = "http_fetch", out = "asset", url = "${read://val:url}")
target(name = "cred", driver = "bash", out = "o.txt", credentials = ["//auth:t"],
       env = {"V": "${read://val:plain}"},
       run = ['echo "token=$TOKEN v=$V" >&2', "exit 3"])
target(name = "badcred", driver = "bash", out = "o.txt", credentials = ["//auth:bad"],
       run = "true")
target(name = "all", driver = "bash", out = "o.txt",
       deps = [":fetch", ":cred", ":badcred"], run = "true > o.txt")
"#,
    );
    ws
}

/// `heph run //app:all --json` over [`workspace`], its slots written.
async fn failed_run(ws: &Workspace) -> JsonRun {
    let m = Matcher::Addr(parse_addr("//app:all").expect("addr"));
    let run = run_json(&ws.engine, &m, &["--json"], None).await;
    assert!(run.res.is_err(), "//app:all must fail");
    ws.engine.failure_slots().writes_drained().await;
    // The fixture has teeth: each culprit failed, for the planted reason.
    for addr in CULPRITS {
        assert_eq!(
            run.entry(addr)["outcome"],
            "failed",
            "{addr}: {:?}",
            run.entries
        );
    }
    let fetch = run.entry("//app:fetch")["error"].to_string();
    assert!(
        fetch.contains("404"),
        "the fetch reached the server: {fetch}"
    );
    let log = std::fs::read_to_string(
        ws.engine
            .failure_slots()
            .slot_dir(&parse_addr("//app:cred").expect("addr"))
            .join("log.txt"),
    )
    .expect("//app:cred's log");
    assert!(
        log.contains("token=[redacted]"),
        "the run printed its credential: {log}"
    );
    run
}

/// Panic naming `what` if `text` holds any planted secret.
fn assert_no_secret(what: &str, text: &str) {
    for s in secrets() {
        assert!(!text.contains(s), "{what} holds {s}:\n{text}");
    }
}

/// `heph inspect failure --json <addr>`.
fn inspect_json(engine: &Engine, addr: &str) -> anyhow::Result<String> {
    let addr = parse_addr(addr)?;
    let view = inspect_view(engine, &addr)?;
    let json = output::JsonArgs::<FailureView>::try_parse_from(["--json"])?;
    let mut buf = Vec::new();
    print_to(
        &json,
        &addr,
        view.as_ref(),
        &mut buf,
        output::Style::Compact,
    )?;
    Ok(String::from_utf8(buf)?)
}

/// I7 on stderr: the CI box and the interactive report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "pending user decision: redact ${read://} values in every run log, or only in the rendered tail"]
async fn no_secret_on_stderr() -> anyhow::Result<()> {
    let ws = workspace();
    let run = failed_run(&ws).await;
    for addr in CULPRITS {
        assert!(run.boxes.contains(addr), "{addr} has a box:\n{}", run.boxes);
        assert!(
            run.report.contains(addr),
            "{addr} is reported:\n{}",
            run.report
        );
    }
    assert_no_secret("the CI boxes", &run.boxes);
    assert_no_secret("the report", &run.report);
    Ok(())
}

/// I7 in `--json`: `run --json`'s records, and `inspect failure --json` of
/// each culprit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_secret_in_json() -> anyhow::Result<()> {
    let ws = workspace();
    let run = failed_run(&ws).await;
    assert_no_secret("run --json", &serde_json::to_string(&run.entries)?);
    for addr in CULPRITS {
        let json = inspect_json(&ws.engine, addr)?;
        assert_ne!(json.trim(), "null", "{addr} has a record");
        assert_no_secret(&format!("inspect failure --json {addr}"), &json);
    }
    Ok(())
}

/// I7 on disk: each culprit's `diagnostic.json`, as written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_secret_in_diagnostic_json() -> anyhow::Result<()> {
    let ws = workspace();
    let _run = failed_run(&ws).await;
    for addr in CULPRITS {
        let path = ws
            .engine
            .failure_slots()
            .slot_dir(&parse_addr(addr)?)
            .join("diagnostic.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("{addr}: {}: {e}", path.display()))?;
        assert_no_secret(&format!("{addr}'s diagnostic.json"), &text);
    }
    Ok(())
}
