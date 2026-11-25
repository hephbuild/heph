#![expect(
    clippy::panic_in_result_fn,
    clippy::let_underscore_must_use,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use common::Workspace;
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A one-shot-per-request loopback HTTP server. Returns its base URL plus a
/// counter of the requests it served — the whole point of caching a fetch is that
/// the second build does not hit the network, and only the server can prove it.
fn serve(body: &'static [u8], max_requests: usize) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let hits = Arc::new(AtomicUsize::new(0));

    let served = Arc::clone(&hits);
    std::thread::spawn(move || {
        for _ in 0..max_requests {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            served.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body);
        }
    });

    (url, hits)
}

const SCRIPT: &[u8] = b"#!/bin/sh\necho fetched-and-executed\n";
/// Deliberately NOT `SCRIPT`'s sha256 (that is `74d5673…`) — the wrong checksum a
/// tampered asset would hash to.
const WRONG_SHA: &str = "e11a1a5ba1b0f6d1a1e5d9b7e0a0f6e0a8b8e9e1e5f0a1b2c3d4e5f6a7b8c9d0";

/// The `http_fetch` driver downloads a URL into a real target output: the fetched
/// file is staged into a consumer's sandbox, executable, and runnable.
#[tokio::test]
async fn test_http_fetch_downloads_an_executable_tool() -> anyhow::Result<()> {
    let (url, hits) = serve(SCRIPT, 4);
    let ws = Workspace::new();
    ws.write_build_file(
        "tool",
        &format!(
            r#"
target(
    name = "dl",
    driver = "http_fetch",
    url = "{url}/tool.sh",
    executable = True,
)
target(
    name = "use",
    driver = "bash",
    run = "$SRC_TOOL > $OUT",
    out = "out.txt",
    deps = {{"tool": ["//tool:dl"]}},
)
"#
        ),
    );

    let result = ws.run("//tool:use").await?;
    assert_eq!(
        common::artifact_string(&result).trim(),
        "fetched-and-executed"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one fetch, one request");
    Ok(())
}

/// A `sha256` mismatch fails the build — the fetched bytes never become an
/// artifact, so a swapped remote asset cannot poison the cache.
#[tokio::test]
async fn test_http_fetch_checksum_mismatch_fails_the_build() -> anyhow::Result<()> {
    let (url, _hits) = serve(SCRIPT, 2);
    let ws = Workspace::new();
    ws.write_build_file(
        "badsum",
        &format!(
            r#"
target(
    name = "dl",
    driver = "http_fetch",
    url = "{url}/tool.sh",
    sha256 = "{WRONG_SHA}",
)
"#
        ),
    );

    let err = ws
        .run("//badsum:dl")
        .await
        .err()
        .expect("a checksum mismatch must fail the build");
    let msg = format!("{err:#}");
    assert!(msg.contains("checksum mismatch"), "got: {msg}");
    Ok(())
}

/// The URL templates over the target's addr args, so one target definition serves
/// every platform — and each renders to its own URL (and its own cache entry).
#[tokio::test]
async fn test_http_fetch_url_templates_over_addr_args() -> anyhow::Result<()> {
    // Two args → two distinct URLs → two requests against the same target def.
    let (url, hits) = serve(SCRIPT, 4);
    let ws = Workspace::new();
    ws.write_build_file(
        "tpl",
        &format!(
            r#"
target(
    name = "dl",
    driver = "http_fetch",
    url = "{url}/tool_{{goos}}.sh",
    executable = True,
)
"#
        ),
    );

    ws.run("//tpl:dl@goos=linux").await?;
    ws.run("//tpl:dl@goos=darwin").await?;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "each arg combination is its own fetch"
    );

    // Re-running an already-fetched combination is a cache hit: no new request.
    ws.run("//tpl:dl@goos=linux").await?;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "a cached fetch must not hit the network again"
    );
    Ok(())
}

/// Serve `body` to one request and hand back the `Authorization` header it
/// carried (empty when none).
fn serve_recording_auth(body: &'static [u8]) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap_or(0);
        let auth = String::from_utf8_lossy(buf.get(..n).unwrap_or_default())
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                    .map(|(_, v)| v.trim().to_string())
            })
            .unwrap_or_default();
        let _ = tx.send(auth);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(body);
    });
    (url, rx)
}

/// `credentials` on `http_fetch`, through the real engine: the host acquires
/// the credential, presents it as a netrc file, and the driver sends that
/// entry's login and password to the URL's host as basic auth.
#[tokio::test]
async fn test_http_fetch_authenticates_with_a_netrc_credential() -> anyhow::Result<()> {
    let (url, auth) = serve_recording_auth(SCRIPT);
    let ws = Workspace::new();
    ws.write_file(
        "secrets/creds.json",
        r#"{"user": "fetcher", "token": "netrc-e2e-token"}"#,
    );
    let root = ws.dir.path().display().to_string();
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(
    name    = "files",
    driver  = "auth.credential",
    sources = [heph.auth.file("{root}/secrets/creds.json",
                              fields = {{"username": "user", "token": "token"}})],
    present = heph.auth.netrc(["127.0.0.1"]),
)
"#
        ),
    );
    ws.write_build_file(
        "dl",
        &format!(
            r#"target(name = "f", driver = "http_fetch", url = "{url}/tool.sh",
       credentials = ["//auth:files"], cache = False)"#
        ),
    );

    ws.run("//dl:f").await?;
    // base64("fetcher:netrc-e2e-token")
    assert_eq!(
        auth.recv()?,
        "Basic ZmV0Y2hlcjpuZXRyYy1lMmUtdG9rZW4=",
        "the netrc entry must reach the server as basic auth"
    );
    Ok(())
}

/// A credential whose netrc names another host fails the fetch before the
/// network, naming the host: never an anonymous request instead.
#[tokio::test]
async fn test_http_fetch_with_credentials_for_another_host_fails() -> anyhow::Result<()> {
    let (url, hits) = serve(SCRIPT, 1);
    let ws = Workspace::new();
    ws.write_file(
        "secrets/creds.json",
        r#"{"user": "fetcher", "token": "netrc-e2e-token"}"#,
    );
    let root = ws.dir.path().display().to_string();
    ws.write_build_file(
        "auth",
        &format!(
            r#"
target(
    name    = "files",
    driver  = "auth.credential",
    sources = [heph.auth.file("{root}/secrets/creds.json",
                              fields = {{"username": "user", "token": "token"}})],
    present = heph.auth.netrc(["files.example.com"]),
)
"#
        ),
    );
    ws.write_build_file(
        "dl",
        &format!(
            r#"target(name = "f", driver = "http_fetch", url = "{url}/tool.sh",
       credentials = ["//auth:files"], cache = False)"#
        ),
    );

    let err = ws
        .run("//dl:f")
        .await
        .err()
        .expect("an uncovered host must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no declared credential covers host \"127.0.0.1\""),
        "{msg}"
    );
    assert!(!msg.contains("netrc-e2e-token"), "material leaked: {msg}");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing reached the server");
    Ok(())
}

/// A fetched binary consumed as a **tool** — the read-only, shared-stage path (what
/// the go plugin's lint/format drivers use for heph-govet) — must still be
/// executable in the consumer's sandbox.
#[tokio::test]
async fn test_http_fetch_tool_is_executable_when_staged_read_only() -> anyhow::Result<()> {
    let (url, _hits) = serve(SCRIPT, 4);
    let ws = Workspace::new();
    ws.write_build_file(
        "rotool",
        &format!(
            r#"
target(
    name = "dl",
    driver = "http_fetch",
    url = "{url}/tool.sh",
    executable = True,
)
target(
    name = "use",
    driver = "bash",
    run = "$TOOL_T > $OUT",
    out = "out.txt",
    tools = {{"t": ["//rotool:dl"]}},
)
"#
        ),
    );

    let result = ws.run("//rotool:use").await?;
    assert_eq!(
        common::artifact_string(&result).trim(),
        "fetched-and-executed"
    );
    Ok(())
}
