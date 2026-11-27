//! Tests for the sticky-comment HTTP layer.
//!
//! This layer is where the feature actually lives — adopt-vs-reset, the section
//! merge, capturing the id from a POST, and every failure mode — and until the
//! [`CommentConfig`] seam existed none of it could be tested at all:
//! `CommentClient::from_env` read `std::env` directly, so constructing one meant
//! mutating global process state.
//!
//! Worse, the branch that *ships* was the branch never exercised. The suite only
//! stayed quiet because `GITHUB_TOKEN` happens to be absent from the test job's
//! environment — an undeclared ambient invariant. Under Actions
//! `GITHUB_REPOSITORY` and `GITHUB_REF` are always set, so the day someone added
//! a token to that job, the tests would have started POSTing comments onto the
//! PR under test.
//!
//! These drive a real loopback HTTP server (the pattern from
//! `crates/e2e/tests/http_fetch.rs` — a std `TcpListener` on `127.0.0.1:0`, no
//! new dependency) so the client's actual request/response handling is covered,
//! not a mock of it.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::tests::{group, pr_only};

/// One request the fake GitHub saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: String,
    /// The request's `If-None-Match` header.
    if_none_match: Option<String>,
}

/// One scripted response.
#[derive(Debug, Clone)]
struct Resp {
    /// The status line, or [`HANG`].
    status: &'static str,
    body: String,
    etag: Option<&'static str>,
}

/// A status that reads the request and never answers it.
const HANG: &str = "HANG";

impl From<(&'static str, String)> for Resp {
    fn from((status, body): (&'static str, String)) -> Self {
        Self {
            status,
            body,
            etag: None,
        }
    }
}

/// `(status, body)` with an `ETag`.
fn tagged(status: &'static str, body: String, etag: &'static str) -> Resp {
    Resp {
        status,
        body,
        etag: Some(etag),
    }
}

/// A scripted GitHub API stand-in.
struct FakeGitHub {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    hits: Arc<AtomicUsize>,
}

impl FakeGitHub {
    /// Serve `responses` in order, one per request; the last is reused once
    /// exhausted. Each entry is `(status_line, json_body)`.
    fn start(responses: Vec<(&'static str, String)>) -> Self {
        Self::serve(responses.into_iter().map(Resp::from).collect())
    }

    /// [`Self::start`] with full control over each response.
    fn serve(responses: Vec<Resp>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));

        let seen_w = Arc::clone(&seen);
        let hits_w = Arc::clone(&hits);
        std::thread::spawn(move || {
            // Sockets of requests left hanging, held open until the test ends.
            let mut held = Vec::new();
            for (i, sock) in listener.incoming().enumerate() {
                let Ok(sock) = sock else { break };
                let resp = responses
                    .get(i)
                    .or_else(|| responses.last())
                    .cloned()
                    .unwrap_or_else(|| ("200 OK", "{}".to_string()).into());
                hits_w.fetch_add(1, Ordering::SeqCst);
                held.extend(handle(sock, &resp, &seen_w));
            }
        });

        Self { url, seen, hits }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// The body of the last write (POST or PATCH), which is the comment as it
    /// would appear on the PR.
    fn last_written_body(&self) -> Option<String> {
        self.requests()
            .iter()
            .rev()
            .find(|r| r.method == "POST" || r.method == "PATCH")
            .map(|r| r.body.clone())
    }
}

/// Record one request and answer it; a [`HANG`] response returns the socket
/// unanswered, for the caller to hold open.
fn handle(
    sock: std::net::TcpStream,
    resp: &Resp,
    seen: &Arc<Mutex<Vec<Seen>>>,
) -> Option<std::net::TcpStream> {
    let mut reader = BufReader::new(sock);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // Headers, to find the body length.
    let mut len = 0usize;
    let mut if_none_match = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => len = value.trim().parse().unwrap_or(0),
            "if-none-match" => if_none_match = Some(value.trim().to_string()),
            _ => {}
        }
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        drop(reader.read_exact(&mut payload));
    }
    // The client sends `{"body": "..."}`; unwrap it so assertions read cleanly.
    let raw = String::from_utf8_lossy(&payload).to_string();
    let body_text = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("body").and_then(|b| b.as_str()).map(str::to_string))
        .unwrap_or(raw);

    seen.lock().unwrap_or_else(|e| e.into_inner()).push(Seen {
        method,
        path,
        body: body_text,
        if_none_match,
    });

    let mut sock = reader.into_inner();
    if resp.status == HANG {
        return Some(sock);
    }
    // A 304 has no body.
    let body = if resp.status.starts_with("304") {
        ""
    } else {
        resp.body.as_str()
    };
    let etag = resp
        .etag
        .map(|e| format!("ETag: {e}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n",
        resp.status,
        body.len()
    );
    drop(sock.write_all(head.as_bytes()));
    drop(sock.write_all(body.as_bytes()));
    drop(sock.flush());
    None
}

fn config(url: &str, section: &str, run_id: &str) -> CommentConfig {
    CommentConfig {
        api_url: url.to_string(),
        repo: "o/r".to_string(),
        token: "t".to_string(),
        pr: 7,
        kind: RunKind::PullRequest,
        tested: Tested::default(),
        repo_url: String::new(),
        job_key: "test".to_string(),
        run_id: run_id.to_string(),
        section_key: section.to_string(),
        timeout: Duration::from_secs(5),
    }
}

/// [`config`] for a merge-queue run.
fn queue_config(url: &str, section: &str, run_id: &str) -> CommentConfig {
    CommentConfig {
        kind: RunKind::MergeQueue,
        ..config(url, section, run_id)
    }
}

/// A comment as GitHub lists it.
fn listed(id: u64, body: &str) -> (&'static str, String) {
    (
        "200 OK",
        serde_json::json!([{"id": id, "body": body}]).to_string(),
    )
}

/// A comment holding both blocks, one section each.
fn both_blocks(pr_run: &str, pr_text: &str, mq_run: &str, mq_text: &str) -> String {
    assemble_body(
        "<!-- heph-gha:test -->",
        "",
        &Groups {
            pull_request: Some(group(pr_run, vec![("sec".into(), pr_text.into())])),
            merge_queue: Some(group(mq_run, vec![("sec".into(), mq_text.into())])),
        },
    )
}

/// A comment holding only the PR block, one section.
fn pr_block(run: &str, text: &str) -> String {
    assemble_body(
        "<!-- heph-gha:test -->",
        "",
        &pr_only(run, vec![("sec".into(), text.into())]),
    )
}

/// A comment as `GET /issues/comments/<id>` returns it.
fn by_id(id: u64, body: &str) -> String {
    serde_json::json!({"id": id, "body": body}).to_string()
}

/// An empty comment list, then a created comment carrying an id.
fn create_flow() -> Vec<(&'static str, String)> {
    vec![
        ("200 OK", "[]".to_string()),
        (
            "201 Created",
            serde_json::json!({"id": 4242, "html_url": "http://example/c"}).to_string(),
        ),
        ("200 OK", "{}".to_string()),
    ]
}

#[test]
fn creates_a_comment_then_patches_the_same_id() {
    let gh = FakeGitHub::start(create_flow());
    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));

    c.sync(|| "first".to_string(), None);
    c.sync(|| "second".to_string(), None);

    let reqs = gh.requests();
    let writes: Vec<&Seen> = reqs
        .iter()
        .filter(|r| r.method == "POST" || r.method == "PATCH")
        .collect();
    assert_eq!(writes.len(), 2, "one create then one edit: {reqs:#?}");
    assert_eq!(writes[0].method, "POST");
    assert_eq!(writes[1].method, "PATCH");
    assert!(
        writes[1].path.contains("/comments/4242"),
        "the id returned by the POST is reused, not a second comment: {}",
        writes[1].path
    );
    assert!(
        writes[1].body.contains("second"),
        "latest content written: {}",
        writes[1].body
    );
}

#[test]
fn adopting_a_same_run_comment_preserves_other_steps() {
    // Another step in this job already wrote its section. Ours must merge, not
    // replace — that is the whole point of the per-step sections.
    let other = assemble_body(
        "<!-- heph-gha:test -->",
        "",
        &pr_only(
            "1-1",
            vec![(
                "othersection".to_string(),
                "## step one\nits results".into(),
            )],
        ),
    );
    let gh = FakeGitHub::start(vec![
        (
            "200 OK",
            serde_json::json!([{"id": 99, "body": other}]).to_string(),
        ),
        ("200 OK", "{}".to_string()),
    ]);

    let c = CommentClient::new(config(&gh.url, "mysection", "1-1"));
    c.sync(|| "## step two\nmy results".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    assert!(body.contains("its results"), "other step kept: {body}");
    assert!(body.contains("my results"), "our section added: {body}");
    assert_eq!(parse_sections(&body).len(), 2, "two sections: {body}");
}

#[test]
fn a_new_run_resets_the_previous_builds_sections() {
    // The comment is reused across runs, but a new run must start clean or the
    // previous build's sections pile up forever.
    let stale = assemble_body(
        "<!-- heph-gha:test -->",
        "",
        &pr_only(
            "1-1",
            vec![
                ("a".to_string(), "old step a".into()),
                ("b".to_string(), "old step b".into()),
            ],
        ),
    );
    let gh = FakeGitHub::start(vec![
        (
            "200 OK",
            serde_json::json!([{"id": 99, "body": stale}]).to_string(),
        ),
        ("200 OK", "{}".to_string()),
    ]);

    // Different run id → reset.
    let c = CommentClient::new(config(&gh.url, "fresh", "2-1"));
    c.sync(|| "new build".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    assert!(!body.contains("old step a"), "stale cleared: {body}");
    assert!(!body.contains("old step b"), "stale cleared: {body}");
    assert!(body.contains("new build"), "{body}");
    assert_eq!(parse_sections(&body).len(), 1, "only ours: {body}");
    assert_eq!(parse_run_id(&body).as_deref(), Some("2-1"));
}

#[test]
fn a_merge_queue_run_adds_its_block_after_the_pr_s_ci() {
    // The PR's CI wrote the comment, under an earlier run. The merge-queue run
    // is a different run, but the PR block is not its to reset.
    let pr_ci = pr_block("1-1", "pr ci results");
    let gh = FakeGitHub::start(vec![listed(99, &pr_ci), ("200 OK", "{}".to_string())]);

    let c = CommentClient::new(queue_config(&gh.url, "sec", "2-1"));
    c.sync(|| "queue results".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    let groups = parse_groups(&body);
    assert_eq!(
        groups.pull_request.as_ref().map(|g| g.run_id.as_str()),
        Some("1-1"),
        "PR block kept under its own run: {body}"
    );
    assert!(body.contains("pr ci results"), "{body}");
    assert_eq!(
        groups.merge_queue.as_ref().map(|g| g.run_id.as_str()),
        Some("2-1"),
        "{body}"
    );
    let pr = body.find("pr ci results").expect("pr");
    let mq = body.find("queue results").expect("queue");
    assert!(pr < mq, "the PR's CI comes first: {body}");
    assert!(body.contains("# Merge queue"), "{body}");
}

#[test]
fn a_merge_queue_re_run_resets_only_its_own_block() {
    let both = both_blocks("1-1", "pr ci results", "2-1", "old queue attempt");
    let gh = FakeGitHub::start(vec![listed(99, &both), ("200 OK", "{}".to_string())]);

    // Same merge-queue run, next attempt.
    let c = CommentClient::new(queue_config(&gh.url, "other", "2-2"));
    c.sync(|| "new queue attempt".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    assert!(body.contains("pr ci results"), "PR block kept: {body}");
    assert!(
        !body.contains("old queue attempt"),
        "stale queue cleared: {body}"
    );
    let mq = parse_groups(&body).merge_queue.expect("queue block");
    assert_eq!(mq.run_id, "2-2");
    assert_eq!(
        mq.sections.len(),
        1,
        "only the new attempt's section: {body}"
    );
}

#[test]
fn a_pr_re_run_resets_only_its_own_block() {
    let both = both_blocks("1-1", "old pr ci", "2-1", "queue results");
    let gh = FakeGitHub::start(vec![listed(99, &both), ("200 OK", "{}".to_string())]);

    let c = CommentClient::new(config(&gh.url, "other", "1-2"));
    c.sync(|| "new pr ci".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    assert!(body.contains("queue results"), "queue block kept: {body}");
    assert!(!body.contains("old pr ci"), "stale PR CI cleared: {body}");
    let pr = parse_groups(&body).pull_request.expect("PR block");
    assert_eq!(pr.run_id, "1-2");
    assert_eq!(pr.sections.len(), 1, "{body}");
}

#[test]
fn every_write_carries_the_other_block_as_it_is_now() {
    // The PR's CI and its merge-queue run overlap. Each read the other's block
    // at adopt; without a re-read before each write, every live tick of one
    // would write that stale copy back over the other's final report.
    let at_adopt = both_blocks("1-1", "pr ci running", "2-1", "queue running");
    let now = both_blocks("1-1", "pr ci running", "2-1", "queue finished");
    let gh = FakeGitHub::start(vec![
        listed(99, &at_adopt),
        ("200 OK", "{}".to_string()),
        ("200 OK", by_id(99, &now)),
        ("200 OK", "{}".to_string()),
    ]);

    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));
    c.sync(|| "pr ci running".to_string(), None);
    c.sync(|| "pr ci still running".to_string(), None);

    let reqs = gh.requests();
    let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(methods, ["GET", "PATCH", "GET", "PATCH"], "{reqs:#?}");
    assert!(
        reqs.get(2)
            .is_some_and(|r| r.path.ends_with("/issues/comments/99")),
        "the comment is re-read by id: {reqs:#?}"
    );
    let body = gh.last_written_body().expect("a write happened");
    assert!(
        body.contains("queue finished"),
        "other block is current: {body}"
    );
    assert!(
        body.contains("pr ci still running"),
        "own block is ours: {body}"
    );
    assert!(
        !body.contains("pr ci running"),
        "own block not re-read: {body}"
    );
}

#[test]
fn an_unchanged_comment_is_re_read_with_its_etag() {
    // Our write's ETag goes into the next re-read; a 304 means nobody wrote
    // since, so the blocks in memory are written back as they are.
    let both = both_blocks("1-1", "pr ci", "2-1", "queue results");
    let gh = FakeGitHub::serve(vec![
        listed(99, &both).into(),
        tagged("200 OK", "{}".to_string(), "W/\"p1\""),
        tagged("304 Not Modified", String::new(), "W/\"p1\""),
        tagged("200 OK", "{}".to_string(), "W/\"p2\""),
        tagged("304 Not Modified", String::new(), "W/\"p2\""),
        ("200 OK", "{}".to_string()).into(),
    ]);

    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));
    c.sync(|| "tick 1".to_string(), None);
    c.sync(|| "tick 2".to_string(), None);
    c.sync(|| "tick 3".to_string(), None);

    let reqs = gh.requests();
    let rereads: Vec<Option<&str>> = reqs
        .iter()
        .filter(|r| r.method == "GET" && r.path.ends_with("/issues/comments/99"))
        .map(|r| r.if_none_match.as_deref())
        .collect();
    assert_eq!(
        rereads,
        [Some("W/\"p1\""), Some("W/\"p2\"")],
        "each re-read carries the last write's ETag: {reqs:#?}"
    );
    let body = gh.last_written_body().expect("a write happened");
    assert!(body.contains("queue results"), "kept on a 304: {body}");
    assert!(body.contains("tick 3"), "{body}");
}

#[test]
fn a_re_read_body_s_etag_is_kept_for_the_next_re_read() {
    // Without an ETag yet the re-read is a plain GET. A 200 brings one; a
    // failed write changes nothing on GitHub, so it stays the one to send.
    let both = both_blocks("1-1", "pr ci", "2-1", "queue v1");
    let v2 = both_blocks("1-1", "pr ci", "2-1", "queue v2");
    let gh = FakeGitHub::serve(vec![
        listed(99, &both).into(),
        ("200 OK", "{}".to_string()).into(),
        tagged("200 OK", by_id(99, &v2), "\"g1\""),
        ("500 Internal Server Error", "{}".to_string()).into(),
        tagged("304 Not Modified", String::new(), "\"g1\""),
        ("200 OK", "{}".to_string()).into(),
    ]);

    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));
    c.sync(|| "tick 1".to_string(), None);
    c.sync(|| "tick 2".to_string(), None);
    c.sync(|| "tick 3".to_string(), None);

    let reqs = gh.requests();
    let rereads: Vec<Option<&str>> = reqs
        .iter()
        .filter(|r| r.method == "GET" && r.path.ends_with("/issues/comments/99"))
        .map(|r| r.if_none_match.as_deref())
        .collect();
    assert_eq!(rereads, [None, Some("\"g1\"")], "{reqs:#?}");
    let body = gh.last_written_body().expect("a write happened");
    assert!(body.contains("queue v2"), "{body}");
    assert!(body.contains("tick 3"), "{body}");
}

#[test]
fn a_re_read_without_the_other_block_drops_it() {
    // The other block is the comment's, not ours: if it is gone from the
    // comment, our write does not bring it back.
    let both = both_blocks("1-1", "pr ci", "2-1", "queue results");
    let gh = FakeGitHub::start(vec![
        listed(99, &both),
        ("200 OK", "{}".to_string()),
        ("200 OK", by_id(99, &pr_block("1-1", "pr ci"))),
        ("200 OK", "{}".to_string()),
    ]);

    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));
    c.sync(|| "tick 1".to_string(), None);
    c.sync(|| "tick 2".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    assert!(!body.contains("queue results"), "{body}");
    assert_eq!(parse_groups(&body).merge_queue, None, "{body}");
    assert!(body.contains("tick 2"), "{body}");
}

#[test]
fn a_failed_re_read_still_writes_the_final_report() {
    let both = both_blocks("1-1", "pr ci running", "2-1", "queue running");
    for failure in ["500 Internal Server Error", "404 Not Found"] {
        let gh = FakeGitHub::start(vec![
            listed(99, &both),
            ("200 OK", "{}".to_string()),
            (failure, "{}".to_string()),
            ("200 OK", "{}".to_string()),
        ]);

        let c = CommentClient::new(config(&gh.url, "sec", "1-1"));
        c.sync(|| "pr ci running".to_string(), None);
        c.sync(|| "pr ci finished".to_string(), Some(CLOSE_SYNC_BUDGET));

        let reqs = gh.requests();
        assert_eq!(
            reqs.last().map(|r| r.method.as_str()),
            Some("PATCH"),
            "{failure}: the write still goes ahead: {reqs:#?}"
        );
        let body = gh.last_written_body().expect("a write happened");
        assert!(body.contains("pr ci finished"), "{failure}: {body}");
        assert!(
            body.contains("queue running"),
            "{failure}: last known queue block kept: {body}"
        );
    }
}

#[test]
fn a_422_leaves_the_client_usable() {
    // Over GitHub's size cap the API answers 422. The old code logged a warning
    // nobody reads and the comment froze for the rest of the job. At minimum the
    // client must not wedge: the next sync still attempts a write.
    let gh = FakeGitHub::start(vec![
        ("200 OK", "[]".to_string()),
        ("422 Unprocessable Entity", "{}".to_string()),
        ("201 Created", serde_json::json!({"id": 5}).to_string()),
    ]);
    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));

    c.sync(|| "too big".to_string(), None);
    let after_first = gh.hits();
    c.sync(|| "retry".to_string(), None);

    assert!(
        gh.hits() > after_first,
        "a failed write must not stop later syncs"
    );
}

#[test]
fn a_404_mid_run_does_not_wedge() {
    // Someone deleted the comment while the build was running.
    let gh = FakeGitHub::start(vec![
        (
            "200 OK",
            serde_json::json!([{"id": 12, "body": "<!-- heph-gha:test -->\n<!-- heph-gha-run:1-1 -->"}])
                .to_string(),
        ),
        ("404 Not Found", "{}".to_string()),
        ("404 Not Found", "{}".to_string()),
    ]);
    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));

    c.sync(|| "one".to_string(), None);
    c.sync(|| "two".to_string(), None);

    assert!(gh.hits() >= 3, "kept trying rather than giving up silently");
}

#[test]
fn the_close_deadline_bounds_a_hanging_api() {
    // `on_close` runs inside the host's drain barrier before process exit, so an
    // unresponsive GitHub must not add its latency to `heph`'s exit.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    // Accept but never respond.
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept() {
            held.push(sock);
        }
    });

    // A per-request timeout far above the deadline: the deadline must bound
    // the request itself, not only whether it starts.
    let mut cfg = config(&url, "sec", "1-1");
    cfg.timeout = Duration::from_secs(30);
    let c = CommentClient::new(cfg);

    let budget = Duration::from_millis(500);
    let start = std::time::Instant::now();
    c.sync(|| "body".to_string(), Some(budget));
    let elapsed = start.elapsed();
    assert!(
        elapsed < budget + WRITE_FLOOR + Duration::from_secs(1),
        "close sync must be bounded, took {elapsed:?}"
    );
}

#[test]
fn the_close_deadline_bounds_the_re_read_and_the_write_together() {
    // A loaded client — the comment was adopted and written — whose API then
    // hangs on every request. The close-time sync re-reads, then writes: each
    // used to get the full per-request timeout, so the close took twice it.
    let both = both_blocks("1-1", "pr ci", "2-1", "queue results");
    let gh = FakeGitHub::serve(vec![
        listed(99, &both).into(),
        ("200 OK", "{}".to_string()).into(),
        (HANG, String::new()).into(),
    ]);
    let mut cfg = config(&gh.url, "sec", "1-1");
    cfg.timeout = Duration::from_secs(30);
    let c = CommentClient::new(cfg);
    c.sync(|| "tick".to_string(), None);

    // The re-read gets half the budget; the write what is left, raised to the
    // floor so it is never skipped.
    let budget = Duration::from_millis(600);
    let start = std::time::Instant::now();
    c.sync(|| "final".to_string(), Some(budget));
    let elapsed = start.elapsed();
    assert!(
        elapsed < budget + WRITE_FLOOR + Duration::from_secs(1),
        "the whole close is bounded by the budget and the write floor, took {elapsed:?}"
    );
    // The server records a request it leaves hanging on its own thread.
    eventually(|| gh.requests().len() >= 4);
    let methods: Vec<String> = gh.requests().into_iter().map(|r| r.method).collect();
    assert_eq!(
        methods,
        ["GET", "PATCH", "GET", "PATCH"],
        "the re-read leaves the write its turn"
    );
}

#[test]
fn a_close_waiting_on_a_hung_live_tick_still_writes_the_closed_render() {
    // The live thread holds the comment lock through a re-read that hangs for
    // its whole per-request timeout. The close waits for the lock; its budget
    // used to start before that wait, so it found no time left, skipped the
    // write, and the comment kept the "running" render for good.
    let both = both_blocks("1-1", "pr ci", "2-1", "queue results");
    let gh = FakeGitHub::serve(vec![
        listed(99, &both).into(),
        ("200 OK", "{}".to_string()).into(),
        (HANG, String::new()).into(),
        ("200 OK", "{}".to_string()).into(),
    ]);
    let mut cfg = config(&gh.url, "sec", "1-1");
    cfg.timeout = Duration::from_millis(800);
    let c = Arc::new(CommentClient::new(cfg));
    c.sync(|| "running".to_string(), None);

    let live = Arc::clone(&c);
    let tick = std::thread::spawn(move || live.sync(|| "still running".to_string(), None));
    // The live tick's re-read is in flight: it holds the lock.
    eventually(|| gh.hits() >= 3);
    c.sync(|| "closed".to_string(), Some(Duration::from_millis(300)));
    drop(tick.join());

    let reqs = gh.requests();
    let last = reqs.last().expect("requests");
    assert_eq!(last.method, "PATCH", "{reqs:#?}");
    assert!(last.body.contains("closed"), "{reqs:#?}");
    assert!(!last.body.contains("running"), "{reqs:#?}");
}

/// Wait up to 5s for `cond`, for state another thread is about to reach.
fn eventually(cond: impl Fn() -> bool) {
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !cond() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_section_key_containing_the_delimiter_does_not_grow_the_body() {
    // The regression that made the body grow until GitHub rejected it: a raw key
    // containing " -->" closed the marker early, so `upsert_section` never
    // matched it again and every tick appended a new section. `section_id`
    // hashes the key, so this must now update in place.
    let key = section_id("run //a:b --> oops");
    let gh = FakeGitHub::start(create_flow());
    let c = CommentClient::new(config(&gh.url, &key, "1-1"));

    for i in 0..5 {
        c.sync(|| format!("tick {i}"), None);
    }

    let body = gh.last_written_body().expect("a write happened");
    assert_eq!(
        parse_sections(&body).len(),
        1,
        "one section after five ticks: {body}"
    );
    assert!(body.contains("tick 4"), "latest content: {body}");
}

#[test]
fn two_matrix_legs_do_not_share_a_comment() {
    // Distinct job keys mean distinct container markers, so neither leg adopts
    // the other's comment and neither can erase it.
    let a = CommentClient::new({
        let mut c = config("http://127.0.0.1:1", "sec", "1-1");
        c.job_key = comment_key("run //...", Some("test".into()), Some("Linux-X64".into()));
        c
    });
    let b = CommentClient::new({
        let mut c = config("http://127.0.0.1:1", "sec", "1-1");
        c.job_key = comment_key("run //...", Some("test".into()), Some("macOS-ARM64".into()));
        c
    });
    assert_ne!(
        a.container_marker, b.container_marker,
        "matrix legs must not share a comment"
    );
}

/// A hook whose comment goes to `c`, with no summary, JSON or annotations.
fn hook_commenting_to(c: CommentClient) -> GhaHook {
    GhaHook {
        inner: Arc::new(Inner {
            tally: Mutex::new(Tally::default()),
            title: "heph run //...".to_string(),
            summary_path: None,
            comment: Some(c),
            stop: AtomicBool::new(false),
            started: AtomicBool::new(false),
            // Long enough that the live thread writes once, at start.
            refresh: Duration::from_secs(3600),
            slow_after_ms: 30_000,
            run_url: None,
            json_path: None,
            annotations: false,
            annotated: Mutex::new(std::collections::HashSet::new()),
            last_flush: Mutex::new(None),
        }),
    }
}

#[test]
fn a_command_that_runs_no_build_posts_no_comment() {
    // The hook loads for every command. `heph tool cache measure-latency` runs
    // no build: no event, and from a cdylib not even an `on_close`. The comment
    // used to be posted at load, so it said "⏳ heph tool" forever.
    let gh = FakeGitHub::start(create_flow());
    let hook = hook_commenting_to(CommentClient::new(config(&gh.url, "sec", "1-1")));
    std::thread::sleep(Duration::from_millis(200));
    hook.on_close();
    assert_eq!(gh.hits(), 0, "no request at all: {:#?}", gh.requests());
}

#[test]
fn a_failed_build_s_last_comment_is_final() {
    // The comment's last write is the live view of the closed tally. With a
    // failure it used to keep saying "still running" after heph had exited.
    let gh = FakeGitHub::start(create_flow());
    let hook = hook_commenting_to(CommentClient::new(config(&gh.url, "sec", "1-1")));
    let ev = |at, kind| BuildEvent {
        at_unix_ms: at,
        kind,
    };
    hook.on_event(&ev(
        1_000,
        hcore::events::BuildEventKind::ResultEnd {
            upstream_file: None,
            path: Vec::new(),
            left_behind: None,
            cancelled: false,
            timed_out: None,
            addr: "//a:broken".into(),
            error: Some("target failed".into()),
            upstream_of: None,
            exit_status: Some("exit status: 1".into()),
            exit_code: None,
            log_tail: None,
        },
    ));
    // Both live writes — the live thread's first tick and the failure's flush,
    // in either order: list + create, then re-read + edit — land before the
    // close, so the close is a write to a loaded comment.
    eventually(|| gh.hits() >= 4);
    assert_eq!(gh.hits(), 4, "{:#?}", gh.requests());
    hook.on_close();
    let body = gh.last_written_body().expect("the comment was written");
    assert!(body.contains("//a:broken"), "{body}");
    assert!(!body.contains("still running"), "{body}");
    assert!(body.contains("finished in"), "{body}");

    // `on_close` re-reads the comment before its write, like every other
    // sync: its write carries the other kind's block as it is now.
    let close: Vec<(String, String)> = gh
        .requests()
        .into_iter()
        .skip(4)
        .map(|r| (r.method, r.path))
        .collect();
    assert_eq!(
        close,
        [
            (
                "GET".to_string(),
                "/repos/o/r/issues/comments/4242".to_string()
            ),
            (
                "PATCH".to_string(),
                "/repos/o/r/issues/comments/4242".to_string()
            ),
        ],
    );
}

#[test]
fn a_merge_queue_run_adopting_its_own_run_keeps_its_other_steps() {
    let body = assemble_body(
        "<!-- heph-gha:test -->",
        "",
        &Groups {
            pull_request: Some(group("1-1", vec![("sec".into(), "pr ci".into())])),
            merge_queue: Some(group(
                "2-1",
                vec![("step1".into(), "queue step one".into())],
            )),
        },
    );
    let gh = FakeGitHub::start(vec![listed(99, &body), ("200 OK", "{}".to_string())]);

    let c = CommentClient::new(queue_config(&gh.url, "step2", "2-1"));
    c.sync(|| "queue step two".to_string(), None);

    let written = gh.last_written_body().expect("a write happened");
    let mq = parse_groups(&written).merge_queue.expect("queue block");
    assert_eq!(
        mq.sections,
        [
            ("step1".to_string(), "queue step one".to_string()),
            ("step2".to_string(), "queue step two".to_string()),
        ],
        "{written}"
    );
    assert!(written.contains("pr ci"), "{written}");
}

#[test]
fn a_merge_queue_run_adopting_a_body_from_before_the_blocks_keeps_it_as_the_pr_s() {
    let legacy = "<!-- heph-gha:test -->\n<!-- heph-gha-run:10-1 -->\n\
                  <!-- heph-gha-step:a -->\nold pr ci\n<!-- /heph-gha-step:a -->";
    let gh = FakeGitHub::start(vec![listed(99, legacy), ("200 OK", "{}".to_string())]);

    let c = CommentClient::new(queue_config(&gh.url, "sec", "2-1"));
    c.sync(|| "queue results".to_string(), None);

    let written = gh.last_written_body().expect("a write happened");
    let groups = parse_groups(&written);
    assert_eq!(
        groups.pull_request,
        Some(group("10-1", vec![("a".into(), "old pr ci".into())])),
        "{written}"
    );
    assert_eq!(
        groups.merge_queue.map(|g| g.sections),
        Some(vec![("sec".to_string(), "queue results".to_string())]),
        "{written}"
    );
}

#[test]
fn the_merge_queue_heading_names_the_base_and_commit_it_tested() {
    let gh = FakeGitHub::start(create_flow());
    let c = CommentClient::new(CommentConfig {
        tested: Tested {
            base: "main".into(),
            sha: "0123456789ab".into(),
        },
        repo_url: "https://github.com/o/r".into(),
        ..queue_config(&gh.url, "sec", "11-2")
    });
    c.sync(|| "queue results".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    assert!(
        body.contains(
            "# Merge queue · `main` · 0123456789ab · \
             [run 11, attempt 2](https://github.com/o/r/actions/runs/11/attempts/2)"
        ),
        "{body}"
    );
}
