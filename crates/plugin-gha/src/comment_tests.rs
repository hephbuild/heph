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

/// One request the fake GitHub saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: String,
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));

        let seen_w = Arc::clone(&seen);
        let hits_w = Arc::clone(&hits);
        std::thread::spawn(move || {
            let mut i = 0usize;
            while let Ok((sock, _)) = listener.accept() {
                let n = hits_w.fetch_add(1, Ordering::SeqCst);
                let (status, body) = responses
                    .get(i)
                    .or_else(|| responses.last())
                    .cloned()
                    .unwrap_or(("200 OK", "{}".to_string()));
                i = i.saturating_add(1);
                let _ = n;
                handle(sock, status, &body, &seen_w);
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

fn handle(sock: std::net::TcpStream, status: &str, body: &str, seen: &Arc<Mutex<Vec<Seen>>>) {
    let mut reader = BufReader::new(sock);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // Headers, to find the body length.
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
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
    });

    let mut sock = reader.into_inner();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    drop(sock.write_all(head.as_bytes()));
    drop(sock.write_all(body.as_bytes()));
    drop(sock.flush());
}

fn config(url: &str, section: &str, run_id: &str) -> CommentConfig {
    CommentConfig {
        api_url: url.to_string(),
        repo: "o/r".to_string(),
        token: "t".to_string(),
        pr: 7,
        kind: RunKind::PullRequest,
        repo_url: None,
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
        None,
        &Groups {
            pull_request: Some(Group {
                run_id: pr_run.into(),
                sections: vec![("sec".into(), pr_text.into())],
            }),
            merge_queue: Some(Group {
                run_id: mq_run.into(),
                sections: vec![("sec".into(), mq_text.into())],
            }),
        },
    )
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
        None,
        &crate::tests::pr_only(
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
        None,
        &crate::tests::pr_only(
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
    let pr_ci = assemble_body(
        "<!-- heph-gha:test -->",
        None,
        &crate::tests::pr_only("1-1", vec![("sec".into(), "pr ci results".into())]),
    );
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
fn the_final_write_carries_the_other_block_as_it_is_now() {
    // The PR's CI and its merge-queue run overlap. Each read the other's block
    // once, at adopt; without a re-read the one finishing last would write that
    // stale copy back over the other's final report.
    let at_adopt = both_blocks("1-1", "pr ci running", "2-1", "queue running");
    let now = both_blocks("1-1", "pr ci running", "2-1", "queue finished");
    let gh = FakeGitHub::start(vec![
        listed(99, &at_adopt),
        ("200 OK", "{}".to_string()),
        (
            "200 OK",
            serde_json::json!({"id": 99, "body": now}).to_string(),
        ),
        ("200 OK", "{}".to_string()),
    ]);

    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));
    c.sync(|| "pr ci running".to_string(), None);
    c.sync_final(|| "pr ci finished".to_string(), None);

    let reqs = gh.requests();
    assert!(
        reqs.iter()
            .any(|r| r.method == "GET" && r.path.ends_with("/issues/comments/99")),
        "the comment is re-read by id: {reqs:#?}"
    );
    let body = gh.last_written_body().expect("a write happened");
    assert!(
        body.contains("queue finished"),
        "other block is current: {body}"
    );
    assert!(body.contains("pr ci finished"), "own block is ours: {body}");
    assert!(
        !body.contains("pr ci running"),
        "own block not re-read: {body}"
    );
}

#[test]
fn a_failed_re_read_still_writes_the_final_report() {
    let both = both_blocks("1-1", "pr ci running", "2-1", "queue running");
    let gh = FakeGitHub::start(vec![
        listed(99, &both),
        ("200 OK", "{}".to_string()),
        ("500 Internal Server Error", "{}".to_string()),
        ("200 OK", "{}".to_string()),
    ]);

    let c = CommentClient::new(config(&gh.url, "sec", "1-1"));
    c.sync(|| "pr ci running".to_string(), None);
    c.sync_final(|| "pr ci finished".to_string(), None);

    let body = gh.last_written_body().expect("a write happened");
    assert!(body.contains("pr ci finished"), "{body}");
    assert!(
        body.contains("queue running"),
        "last known queue block kept: {body}"
    );
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

    let mut cfg = config(&url, "sec", "1-1");
    cfg.timeout = Duration::from_millis(300);
    let c = CommentClient::new(cfg);

    let start = std::time::Instant::now();
    c.sync(
        || "body".to_string(),
        Some(std::time::Instant::now() + Duration::from_millis(500)),
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "close sync must be bounded, took {elapsed:?}"
    );
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
            left_behind: None,
            cancelled: false,
            timed_out: None,
            addr: "//a:broken".into(),
            error: Some("target failed".into()),
            upstream_of: None,
            exit_status: Some("exit status: 1".into()),
            log_tail: None,
        },
    ));
    hook.on_close();
    let body = gh.last_written_body().expect("the comment was written");
    assert!(body.contains("//a:broken"), "{body}");
    assert!(!body.contains("still running"), "{body}");
    assert!(body.contains("finished in"), "{body}");
}
