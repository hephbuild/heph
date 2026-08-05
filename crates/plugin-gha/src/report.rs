//! Machine-readable surfaces: workflow annotations, the JSON document, and the
//! `GITHUB_OUTPUT` scalars.
//!
//! Three consumers, three shapes:
//!
//! - **`::error::` annotations** reach a *human* while the job is still running.
//!   They stream onto the run page and appear inline in the job log at the point
//!   of failure, cost zero API calls, and are immune to rate limits. Nothing else
//!   in Actions has all four properties, which is why they are the primary live
//!   channel rather than a nicety.
//! - **The JSON document** is what an agent reads to decide what to do next.
//!   Unbounded, written to a file.
//! - **The embedded JSON** is a *different, hard-capped* document for an agent
//!   that can only fetch the comment through the API. See [`EMBED_MAX`].

use crate::render::fmt_duration;
use crate::tally::{Counters, RootFailure, Tally};

/// Schema identifier. Field names never change within a version.
pub(crate) const SCHEMA: &str = "heph.gha/1";

/// Hard cap on the JSON embedded in a comment body.
///
/// The embed is **not** the same document as the file. It lives inside a body
/// GitHub caps at 65,536 characters — a budget already shared with the header,
/// the failure boxes, and every other step's section in the same job. At 20k
/// targets a naive embed of the full document is tens of KiB on its own and
/// would consume the entire comment.
///
/// So the embed carries counters, status, elapsed and **root addrs only** — no
/// log tails, no slowest list, no package rollups — and is budgeted *first*,
/// before any prose. That ordering matters: a truncated *fact* is recoverable for
/// a human, but a truncated JSON document is **unparseable**, which breaks the
/// very agent that depends on it.
pub(crate) const EMBED_MAX: usize = 2048;

/// Longest addr allowed into the embed.
const EMBED_ADDR_MAX: usize = 128;

/// The marker wrapping the embedded document.
pub(crate) const EMBED_OPEN: &str = "<!-- heph:json ";
pub(crate) const EMBED_CLOSE: &str = " -->";

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

fn status_str(t: &Tally, c: &Counters) -> &'static str {
    if c.roots_total > 0 {
        "failed"
    } else if t.status_emoji() == "✅" {
        "ok"
    } else {
        "running"
    }
}

/// The full document — unbounded, for `jsonPath`.
///
/// `failures` and `slowest` are sorted deterministically so two runs diff
/// cleanly, and **collateral failures never appear**: they are `blocked_count`
/// on their root. An agent reading this must not have to re-derive the
/// root/collateral split any more than a human does.
pub(crate) fn full_document(
    t: &Tally,
    c: &Counters,
    elapsed_ms: u64,
    command: &str,
    json_path: Option<&str>,
) -> serde_json::Value {
    let (done, matched, _) = t.progress();
    let failures: Vec<serde_json::Value> = t.roots().iter().map(root_json).collect();
    let slowest: Vec<serde_json::Value> = t
        .slowest()
        .iter()
        .map(|s| {
            serde_json::json!({
                "addr": s.addr,
                "driver": s.driver,
                "duration_ms": s.duration_ms,
            })
        })
        .collect();

    serde_json::json!({
        "schema": SCHEMA,
        "status": status_str(t, c),
        "command": command,
        "elapsed_ms": elapsed_ms,
        "fail_fast": t.fail_fast(),
        "truncated": false,
        "json_path": json_path,
        "targets": {
            "matched": matched,
            "done": done,
            "failed": c.roots_total,
            "blocked": c.blocked,
            "cached": c.cached(),
            "executed": c.executed,
        },
        "cache": {
            "local_hits": c.cached_local,
            "remote_hits": c.cached_remote,
            "misses": c.misses(),
            "hit_rate": c.hit_rate(),
        },
        "failures": failures,
        "slowest": slowest,
    })
}

fn root_json(r: &RootFailure) -> serde_json::Value {
    serde_json::json!({
        "addr": r.addr,
        "driver": r.driver,
        "duration_ms": r.duration_ms,
        "exit_status": r.exit_status,
        "blocked_count": r.blocked,
        "message": r.message,
        "log_tail": r.log_tail.as_ref().map(|l| &l.text),
    })
}

/// The compact document embedded in a comment body, guaranteed to fit
/// [`EMBED_MAX`] **and to parse**.
///
/// Root addrs are dropped oldest-first until it fits, and what was dropped is
/// recorded *inside* the JSON — an agent reading a truncated view must be able
/// to tell that it is truncated, and to find the complete document.
/// `truncated` and `json_path` are therefore always present.
pub(crate) fn embedded_document(
    t: &Tally,
    c: &Counters,
    elapsed_ms: u64,
    json_path: Option<&str>,
) -> String {
    let (_, matched, _) = t.progress();
    let mut addrs: Vec<String> = t
        .roots()
        .iter()
        .map(|r| truncate_chars(&r.addr, EMBED_ADDR_MAX))
        .collect();
    let mut omitted = c.roots_total.saturating_sub(addrs.len());

    loop {
        let doc = serde_json::json!({
            "schema": SCHEMA,
            "status": status_str(t, c),
            "elapsed_ms": elapsed_ms,
            "truncated": omitted > 0,
            "failures_omitted": omitted,
            "json_path": json_path,
            "targets": {
                "matched": matched,
                "failed": c.roots_total,
                "blocked": c.blocked,
                "cached": c.cached(),
                "executed": c.executed,
            },
            "failures": addrs,
        });
        // Compact, never pretty: every byte here is taken from the prose budget.
        let s = serde_json::to_string(&doc).unwrap_or_else(|_| "{}".to_string());
        if s.len() <= EMBED_MAX || addrs.is_empty() {
            return s;
        }
        // Drop the oldest root and record it as omitted.
        addrs.remove(0);
        omitted = omitted.saturating_add(1);
    }
}

/// Wrap the embed in its HTML-comment marker.
pub(crate) fn embed_marker(json: &str) -> String {
    format!("{EMBED_OPEN}{json}{EMBED_CLOSE}")
}

/// `::error::` workflow commands, one per **root** failure.
///
/// Capped at the root count and never emitted for collateral: at 20k targets one
/// broken leaf blocks thousands, and an annotation each would be 4,000+ lines of
/// noise burying the one that matters.
///
/// Target-level only — no `file=`/`line=`. heph cannot produce them:
/// `TargetFailure` carries an addr, a log tail and a cause chain, nothing more.
/// Regexing `file:line` out of the log tail here would be a zoo of per-language
/// heuristics living in a reporter, producing *wrong* annotations on the PR diff,
/// which is worse than none. That waits on driver-emitted structured diagnostics.
pub(crate) fn annotations(roots: &[RootFailure], limit: usize) -> Vec<String> {
    roots
        .iter()
        .take(limit)
        .map(|r| {
            let mut detail = String::new();
            if let Some(status) = &r.exit_status {
                detail.push_str(status);
                detail.push_str(": ");
            }
            detail.push_str(r.message.lines().next().unwrap_or("failed"));
            if let Some(tail) = &r.log_tail
                && let Some(last) = tail.text.lines().rev().find(|l| !l.trim().is_empty())
            {
                detail.push_str(" — ");
                detail.push_str(last);
            }
            if r.blocked > 0 {
                detail.push_str(&format!(" ({} targets blocked)", r.blocked));
            }
            format!(
                "::error title=heph {}::{}",
                escape_annotation(&r.addr),
                escape_annotation(&truncate_chars(&detail, 400))
            )
        })
        .collect()
}

/// Escape the characters that would terminate or corrupt a workflow command.
///
/// A newline ends the command, so an unescaped multi-line message would spill
/// the remainder into the log as literal text — and, worse, anything after a
/// `::` in it could be interpreted as a *new* workflow command. The log tail is
/// whatever the target printed, so it is untrusted input in this position.
fn escape_annotation(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
        .replace("::", "%3A%3A")
}

/// `key=value` lines for `$GITHUB_OUTPUT` — the GHA-native agent surface: free,
/// no parsing, no rate limit, and readable by a downstream step as
/// `${{ steps.build.outputs.heph_failed }}`.
pub(crate) fn github_outputs(
    t: &Tally,
    c: &Counters,
    elapsed_ms: u64,
    json_path: Option<&str>,
) -> Vec<String> {
    let mut out = vec![
        format!("heph_status={}", status_str(t, c)),
        format!("heph_failed={}", c.roots_total),
        format!("heph_blocked={}", c.blocked),
        format!("heph_executed={}", c.executed),
        format!("heph_cached={}", c.cached()),
        format!("heph_elapsed_ms={elapsed_ms}"),
        format!("heph_elapsed={}", fmt_duration(elapsed_ms)),
    ];
    if let Some(rate) = c.hit_rate() {
        out.push(format!("heph_cache_hit_rate={rate:.4}"));
    }
    if let Some(p) = json_path {
        out.push(format!("heph_json_path={p}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::events::{BuildEvent, BuildEventKind, LogTailData};

    fn ev(at: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent {
            at_unix_ms: at,
            kind,
        }
    }

    fn failing(roots: usize, blocked: usize, msg_len: usize) -> Tally {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: (0..20_000)
                    .map(|i| format!("//pkg{}:t{i}", i % 50))
                    .collect(),
                complete: true,
            },
        ));
        let msg = "e".repeat(msg_len);
        for i in 0..roots {
            t.apply(&ev(
                1_000,
                BuildEventKind::ResultEnd {
                    addr: format!("//very/long/package/path/number{i}:some-broken-target"),
                    error: Some(msg.clone()),
                    upstream_of: None,
                    exit_status: Some("exit status: 1".into()),
                    log_tail: Some(LogTailData {
                        text: "FAIL\nline two".into(),
                        start_line: 1,
                    }),
                },
            ));
        }
        for i in 0..blocked {
            t.apply(&ev(
                2_000,
                BuildEventKind::ResultEnd {
                    addr: format!("//svc:t{i}"),
                    error: Some("dependency failed".into()),
                    upstream_of: Some("//very/long/package/path/number0:some-broken-target".into()),
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        t
    }

    #[test]
    fn the_embed_always_fits_and_always_parses() {
        // The case that matters: many roots with huge messages, at 20k targets.
        // A truncated fact is survivable; a truncated JSON object is not — it
        // breaks the agent that depends on it.
        for roots in [0, 1, 10, 200] {
            let t = failing(roots, 4_117, 4_096);
            let c = t.counters();
            let s = embedded_document(&t, &c, 468_000, Some("/tmp/h.json"));
            assert!(
                s.len() <= EMBED_MAX,
                "embed over cap with {roots} roots: {} bytes",
                s.len()
            );
            let v: serde_json::Value =
                serde_json::from_str(&s).unwrap_or_else(|e| panic!("must parse ({roots}): {e}"));
            assert_eq!(v["schema"], SCHEMA);
            // Mandatory regardless of truncation — without them a truncated view
            // is a dead end.
            assert!(v.get("truncated").is_some(), "truncated always present");
            assert_eq!(
                v["json_path"], "/tmp/h.json",
                "always points at the full doc"
            );
        }
    }

    #[test]
    fn a_truncated_embed_says_so_and_counts_what_it_dropped() {
        let t = failing(10, 0, 4_096);
        let c = t.counters();
        let s = embedded_document(&t, &c, 1_000, Some("/tmp/h.json"));
        let v: serde_json::Value = serde_json::from_str(&s).expect("parses");
        // Roots are capped at ROOTS_KEPT=10 in the tally and further trimmed here
        // by the byte budget; either way the count must stay exact.
        assert_eq!(v["targets"]["failed"], 10, "true count is never lost");
        if v["truncated"] == serde_json::Value::Bool(true) {
            assert!(
                v["failures_omitted"].as_u64().unwrap_or(0) > 0,
                "omission is quantified: {s}"
            );
        }
    }

    #[test]
    fn the_embed_carries_no_log_tails() {
        // The whole reason it is a separate document from the file.
        let t = failing(1, 0, 32);
        let c = t.counters();
        let s = embedded_document(&t, &c, 1_000, None);
        assert!(!s.contains("log_tail"), "no tails in the embed: {s}");
        assert!(!s.contains("slowest"), "no slowest list in the embed: {s}");
    }

    #[test]
    fn the_full_document_separates_roots_from_collateral() {
        let t = failing(2, 4_117, 32);
        let c = t.counters();
        let v = full_document(&t, &c, 468_000, "run //...", None);
        let failures = v["failures"].as_array().expect("failures array");
        assert_eq!(failures.len(), 2, "collateral is never a failure entry");
        assert_eq!(v["targets"]["blocked"], 4_117);
        assert_eq!(
            failures[0]["blocked_count"], 4_117,
            "collateral attributed to its root"
        );
        assert!(failures[0]["log_tail"].is_string(), "file doc keeps tails");
    }

    #[test]
    fn annotations_are_one_per_root_never_per_collateral() {
        // One broken leaf blocking 4,117 targets must not produce 4,117
        // annotations.
        let t = failing(2, 4_117, 32);
        let a = annotations(t.roots(), 10);
        assert_eq!(a.len(), 2, "one per root");
        assert!(
            a[0].starts_with("::error title=heph //very/long"),
            "{}",
            a[0]
        );
        assert!(a[0].contains("4117 targets blocked"), "{}", a[0]);
    }

    #[test]
    fn an_annotation_cannot_be_broken_by_target_output() {
        // The log tail is whatever the target printed — untrusted in this
        // position. A newline would end the workflow command, and a `::` could
        // start a new one.
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: Some("boom".into()),
                upstream_of: None,
                exit_status: None,
                // The injection attempt is the LAST line, which is the one the
                // annotation actually quotes.
                log_tail: Some(LogTailData {
                    text: "safe 100% done\n::error title=spoofed::not really failing".into(),
                    start_line: 1,
                }),
            },
        ));
        let a = annotations(t.roots(), 10);
        let line = a.first().expect("one annotation");
        assert_eq!(line.lines().count(), 1, "single line: {line}");
        assert!(!line.contains('\n'), "no raw newline: {line}");
        // A workflow command is `::error title=X::message` — exactly two `::` of
        // its own. Any more would mean the payload injected one.
        assert_eq!(
            line.matches("::").count(),
            2,
            "payload contributed no command separator: {line}"
        );
        assert!(line.contains("%3A%3A"), "payload `::` escaped: {line}");
        assert!(
            !line.contains("::error title=spoofed"),
            "cannot spoof a second annotation: {line}"
        );
    }

    #[test]
    fn github_outputs_are_scalar_and_parseable() {
        let t = failing(2, 10, 32);
        let c = t.counters();
        let out = github_outputs(&t, &c, 468_000, Some("/tmp/h.json"));
        let joined = out.join("\n");
        assert!(joined.contains("heph_status=failed"), "{joined}");
        assert!(joined.contains("heph_failed=2"), "{joined}");
        assert!(joined.contains("heph_blocked=10"), "{joined}");
        assert!(joined.contains("heph_json_path=/tmp/h.json"), "{joined}");
        for line in &out {
            assert_eq!(line.lines().count(), 1, "one line per key: {line}");
            assert!(line.contains('='), "key=value: {line}");
        }
    }

    #[test]
    fn hit_rate_is_absent_rather_than_zero_when_never_consulted() {
        let t = failing(0, 0, 0);
        let c = t.counters();
        let out = github_outputs(&t, &c, 1_000, None);
        assert!(
            !out.iter().any(|l| l.starts_with("heph_cache_hit_rate")),
            "no key at all beats a misleading 0: {out:?}"
        );
        let v = full_document(&t, &c, 1_000, "run //...", None);
        assert!(v["cache"]["hit_rate"].is_null());
    }
}
