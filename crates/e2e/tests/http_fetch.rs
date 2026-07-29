#![expect(
    clippy::panic_in_result_fn,
    clippy::let_underscore_must_use,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

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
