//! Markdown renderers for the GitHub Actions hook.
//!
//! Two renderers, not one, because the live comment and the final step summary
//! are different products (see `docs/GHA_REPORTING.md` §2):
//!
//! - [`render_live`] answers *pass? stuck? how long? is it mine?* for someone
//!   glancing at a PR comment on a phone. ≤8 lines, failures first, no wide
//!   tables, and it self-dates so a stale comment is distinguishable from a slow
//!   build.
//! - [`render_final`] answers *what broke and why, how long, what did cache do*
//!   for someone debugging a red build, and for an agent deciding what to do
//!   next.
//!
//! Reusing one renderer for both is what made the old summary ship a
//! "slow targets" table that was structurally near-empty: it reported
//! *currently-running* targets at the moment everything had finished.
//!
//! # Byte budgets
//!
//! Every render takes a **byte budget** and is guaranteed not to exceed it.
//! This is a correctness requirement, not tidiness: GitHub caps an issue comment
//! at 65,536 characters, and over that it answers 422 — which the old code
//! turned into a `tracing::warn!` nobody reads, leaving the comment frozen at its
//! last good body for the rest of the job. It broke exactly when the report
//! mattered most, because the thing that blew the budget was having many
//! failures.
//!
//! Row *counts* are not a budget: a failure message is
//! `format!("{e:#}")` over an anyhow chain, which is the whole cause chain on one
//! line, unbounded in width. So every variable-length field is capped in
//! characters, sections are emitted in priority order, and anything dropped says
//! so.

use crate::tally::{Counters, Tally};

/// GitHub's hard cap on an issue-comment body.
pub(crate) const COMMENT_LIMIT: usize = 65_536;

/// GitHub's hard cap on `$GITHUB_STEP_SUMMARY`. Rendered with headroom.
pub(crate) const SUMMARY_BUDGET: usize = 900 * 1024;

/// Budget for **one step's section** of the shared comment.
///
/// A job's comment holds a section per heph step, all concatenated by
/// `assemble_body`, plus the hidden markers. Budgeting a single section at the
/// full comment limit would let one step consume the whole body and 422 the
/// next; a quarter leaves room for several steps and the markers.
pub(crate) const LIVE_SECTION_BUDGET: usize = COMMENT_LIMIT / 4;

/// Longest single failure message rendered. The full text is in the job log and
/// the JSON document; the report is an index, not a transcript.
const MAX_MSG_CHARS: usize = 200;

/// Longest addr rendered inline.
const MAX_ADDR_CHARS: usize = 128;

/// Live view: sample sizes. Small on purpose — on a phone the count is the
/// signal and the rows are the illustration.
const LIVE_RUNNING_ROWS: usize = 6;
const LIVE_LOCK_ROWS: usize = 3;
const LIVE_ROOT_ROWS: usize = 5;
/// Log-tail lines shown live, for the first root only.
const LIVE_LOG_LINES: usize = 5;
const LIVE_LOG_BYTES: usize = 1024;

/// Final view: table sizes.
const FINAL_SLOWEST_ROWS: usize = 20;
const FINAL_PKG_ROWS: usize = 15;
/// Log-tail budget in the final report: generous, since the summary has ~900 KiB
/// and this is what the reader came for — but still bounded, because the tail is
/// attacker-shaped data (it is whatever the target printed).
const FINAL_LOG_BYTES: usize = 8 * 1024;

/// A `String` that will not grow past a byte budget.
///
/// Sections push into it and check the return value: a section that does not fit
/// is skipped whole rather than half-written, so the output is never a truncated
/// table row or an unterminated `<details>`.
pub(crate) struct Budgeted {
    out: String,
    budget: usize,
    truncated: bool,
}

impl Budgeted {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            out: String::new(),
            budget,
            truncated: false,
        }
    }

    /// Push if it fits. Returns whether it was written.
    fn push(&mut self, s: &str) -> bool {
        // Reserve room for the truncation notice so it can always be appended.
        const RESERVE: usize = 64;
        if self
            .out
            .len()
            .saturating_add(s.len())
            .saturating_add(RESERVE)
            > self.budget
        {
            self.truncated = true;
            return false;
        }
        self.out.push_str(s);
        true
    }

    pub(crate) fn finish(mut self) -> String {
        if self.truncated {
            self.out.push_str("\n_…output truncated to fit._\n");
        }
        self.out
    }
}

/// Truncate to at most `max` characters, on a character boundary, with an
/// ellipsis when anything was dropped.
///
/// Character-counting rather than byte-slicing is required, not stylistic: an
/// addr or an error message can contain multi-byte UTF-8, and a byte-index slice
/// through one is a panic. The workspace also denies raw string slicing for
/// exactly this reason.
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

/// Collapse a message to its first line, capped. `format!("{e:#}")` puts the
/// whole anyhow chain on one line, so this is a width cap, not a line cap.
fn one_line(msg: &str, max: usize) -> String {
    let first = msg.lines().next().unwrap_or("");
    truncate_chars(first.trim(), max)
}

fn addr_cell(addr: &str) -> String {
    truncate_chars(addr, MAX_ADDR_CHARS)
}

/// `4m12s`, `58s`, `1h04m`.
pub(crate) fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3_600 {
        return format!("{}m{:02}s", secs / 60, secs % 60);
    }
    format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
}

/// `1,412` — thousands separators, because the difference between 4117 and 41171
/// is not scannable at a glance and these numbers are read at a glance.
fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let len = s.chars().count();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A 20-cell progress bar. Unicode blocks rather than an image: a badge would be
/// an external HTTP dependency inside a build report.
fn progress_bar(done: usize, total: usize) -> String {
    const CELLS: usize = 20;
    if total == 0 {
        return "░".repeat(CELLS);
    }
    let filled = (done.saturating_mul(CELLS))
        .saturating_div(total)
        .min(CELLS);
    let mut s = String::with_capacity(CELLS * 3);
    for i in 0..CELLS {
        s.push(if i < filled { '█' } else { '░' });
    }
    s
}

/// `updated 14:02:11Z`, quantized to 5-second granularity.
///
/// Quantization is what makes unchanged-body suppression possible at all: an
/// unquantized clock changes every render, so every timer tick would PATCH even
/// when nothing about the build changed.
pub(crate) fn fmt_clock(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) / 5 * 5;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}Z")
}

/// Inputs both renderers share.
pub(crate) struct RenderCtx<'a> {
    pub heading: &'a str,
    pub now_ms: u64,
    pub slow_after_ms: u64,
    /// A link to the workflow run, when one can be built from the environment.
    pub run_url: Option<&'a str>,
}

fn counts_line(t: &Tally, c: &Counters) -> String {
    let (done, total, complete) = t.progress();
    let total_str = if complete {
        fmt_count(total)
    } else {
        format!("~{}", fmt_count(total))
    };
    format!(
        "**{} / {}** done · {} cached · {} executed · **{} failed**\n",
        fmt_count(done),
        total_str,
        fmt_count(c.cached()),
        fmt_count(c.executed),
        fmt_count(c.roots_total),
    )
}

/// Render the **live** view — the sticky PR comment while the build runs.
///
/// Ordering is most-alarming-first: failures come above the counts, because they
/// are the reason anyone opens the comment. Only *root* failures are listed;
/// collateral is a number. At 20k targets one broken leaf can block thousands of
/// targets, and rendering a row each would push the actual cause off the bottom
/// of a phone screen.
pub(crate) fn render_live(t: &Tally, ctx: &RenderCtx<'_>, budget: usize) -> String {
    let mut b = Budgeted::new(budget);
    let c = t.counters();
    let elapsed = t.elapsed_ms(ctx.now_ms);

    let status = if c.roots_total > 0 {
        format!(
            "## ❌ {} · **{} failed**{} · still running ({})\n\n",
            ctx.heading,
            fmt_count(c.roots_total),
            if c.blocked > 0 {
                format!(" (+{} blocked)", fmt_count(c.blocked))
            } else {
                String::new()
            },
            fmt_duration(elapsed),
        )
    } else {
        format!(
            "## {} {} · {}\n\n",
            t.status_emoji(),
            ctx.heading,
            fmt_duration(elapsed)
        )
    };
    b.push(&status);

    // Failures first.
    if c.roots_total > 0 {
        push_live_failures(&mut b, t, &c);
    }

    // Counts + the freshness clock.
    b.push(&counts_line(t, &c));
    let (done, total, _) = t.progress();
    let workers = if t.max_workers() > 0 {
        format!(
            " · {}/{} workers busy",
            fmt_count(t.active_foreground().min(t.max_workers())),
            fmt_count(t.max_workers())
        )
    } else {
        String::new()
    };
    let pct = if total > 0 {
        done.saturating_mul(100).saturating_div(total)
    } else {
        0
    };
    b.push(&format!(
        "`{}` {}%{} · updated {}\n",
        progress_bar(done, total),
        pct,
        workers,
        fmt_clock(ctx.now_ms),
    ));

    if t.background_ops() > 0 {
        // One aggregate row, never per-target: `RemoteCacheWriteStart` fires
        // before the upload semaphore is acquired, so at 20k targets this is
        // ~20k entries with 16 actually uploading.
        b.push(&format!(
            "↑ {} cache uploads in flight (background)\n",
            fmt_count(t.background_ops())
        ));
    }

    if t.fail_fast() && c.roots_total > 0 {
        b.push(
            "\n> [!WARNING]\n> Stopped at the first failure (`--fail-fast`) — \
             remaining targets were not attempted.\n",
        );
    }

    push_lock_waits(&mut b, t, ctx);
    push_running_longest(&mut b, t, ctx);

    if c.roots_total > 0 {
        b.push("\n_Build continues — remaining targets still running._\n");
    }

    b.finish()
}

fn push_live_failures(b: &mut Budgeted, t: &Tally, c: &Counters) {
    let roots = t.roots();
    let mut alert = format!(
        "> [!CAUTION]\n> **{} root failure{}**",
        fmt_count(c.roots_total),
        if c.roots_total == 1 { "" } else { "s" }
    );
    if c.blocked > 0 {
        alert.push_str(&format!(
            " — {} targets blocked downstream",
            fmt_count(c.blocked)
        ));
    }
    alert.push_str(".\n\n");
    b.push(&alert);

    for (i, r) in roots.iter().take(LIVE_ROOT_ROWS).enumerate() {
        let dur = r
            .duration_ms
            .map(|d| format!(", after {}", fmt_duration(d)))
            .unwrap_or_default();
        let mut section = format!(
            "### `{}` — failed{}\n\n{}\n",
            addr_cell(&r.addr),
            dur,
            one_line(&r.message, MAX_MSG_CHARS)
        );
        // Only the first root gets a log tail live: mid-build the reader is
        // answering "is this mine?", and one error answers that where ten don't.
        if i == 0
            && let Some(tail) = r.log_tail.as_ref().and_then(live_log_tail)
        {
            section.push_str(&format!("\n```\n{tail}\n```\n"));
        }
        section.push_str(&format!("\nReproduce: `heph run {}`\n", addr_cell(&r.addr)));
        if r.blocked > 0 {
            section.push_str(&format!(
                "**{} targets blocked** — {}\n",
                fmt_count(r.blocked),
                pkg_rollup(r, 3)
            ));
        }
        section.push('\n');
        if !b.push(&section) {
            break;
        }
    }
    if c.roots_total > roots.len() {
        b.push(&format!(
            "…and {} more root failures.\n\n",
            fmt_count(c.roots_total.saturating_sub(roots.len()))
        ));
    }
}

/// The last few lines of a failing target's log, bounded for the live view.
///
/// Mid-build the reader is answering "is this mine?", which one error answers
/// and ten don't — so this is deliberately shorter than the final view's tail,
/// and only the first root gets one. The byte cap matters independently of the
/// line cap: a single line of a minified bundle or a base64 blob can be
/// megabytes on its own.
fn live_log_tail(tail: &hcore::events::LogTailData) -> Option<String> {
    bounded_tail(&tail.text, LIVE_LOG_LINES, LIVE_LOG_BYTES)
}

/// The full retained tail for the final report, bounded in bytes only — the
/// engine already applied the user's `--log-lines`, so a line cap here would
/// silently override their choice.
fn final_log_tail(tail: &hcore::events::LogTailData) -> Option<String> {
    bounded_tail(&tail.text, usize::MAX, FINAL_LOG_BYTES)
}

/// The last `max_lines` lines of `text`, in order, stopping before `max_bytes`.
///
/// Both caps are load-bearing. A line cap alone does not bound the output: one
/// line of a minified bundle, a base64 blob, or a stack trace with a huge inline
/// value can be megabytes by itself. A byte cap alone would cut mid-line.
fn bounded_tail(text: &str, max_lines: usize, max_bytes: usize) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    // Walk backwards so the *end* of the log survives — that is where the error
    // is — then restore order.
    for line in lines.get(start..).unwrap_or(&[]).iter().rev() {
        let cost = line.len().saturating_add(1);
        if bytes.saturating_add(cost) > max_bytes {
            break;
        }
        bytes = bytes.saturating_add(cost);
        kept.push(line);
    }
    kept.reverse();
    let out = kept.join("\n");
    (!out.trim().is_empty()).then_some(out)
}

fn pkg_rollup(r: &crate::tally::RootFailure, limit: usize) -> String {
    let mut v: Vec<(&str, usize)> = r
        .blocked_by_pkg
        .iter()
        .map(|(k, n)| (k.as_ref(), *n))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let shown: Vec<String> = v
        .iter()
        .take(limit)
        .map(|(p, n)| {
            format!(
                "`{}` ({})",
                truncate_chars(p, MAX_ADDR_CHARS),
                fmt_count(*n)
            )
        })
        .collect();
    let mut s = shown.join(", ");
    if v.len() > limit {
        s.push_str(&format!(
            ", …{} more packages",
            v.len().saturating_sub(limit)
        ));
    }
    s
}

fn push_lock_waits(b: &mut Budgeted, t: &Tally, ctx: &RenderCtx<'_>) {
    let (waits, total) = t.lock_waits(ctx.now_ms, ctx.slow_after_ms, LIVE_LOCK_ROWS);
    if waits.is_empty() {
        return;
    }
    b.push(&format!(
        "\n> [!WARNING]\n> {} target{} waiting on the result lock — \
         another heph process holds it.\n\n",
        fmt_count(total),
        if total == 1 { "" } else { "s" }
    ));
    b.push("| target | held by | waiting |\n| --- | --- | --- |\n");
    for w in &waits {
        let holder = match w.holder_pid {
            Some(pid) => format!("pid {pid}"),
            None => "unknown".to_string(),
        };
        let row = format!(
            "| `{}` | {} | {} |\n",
            addr_cell(&w.addr),
            holder,
            fmt_duration(ctx.now_ms.saturating_sub(w.since_ms))
        );
        if !b.push(&row) {
            break;
        }
    }
}

fn push_running_longest(b: &mut Budgeted, t: &Tally, ctx: &RenderCtx<'_>) {
    let (rows, total) = t.running_longest(ctx.now_ms, ctx.slow_after_ms, LIVE_RUNNING_ROWS);
    if rows.is_empty() {
        return;
    }
    // The count is the honest signal; the rows are a sample. At 20k targets
    // "218 over 30s" is the fact — listing 218 is not an option.
    b.push(&format!(
        "\n<details><summary>Running longest ({} of {} over {})</summary>\n\n\
         | target | phase | for |\n| --- | --- | --- |\n",
        rows.len(),
        fmt_count(total),
        fmt_duration(ctx.slow_after_ms),
    ));
    for (addr, phase, elapsed) in &rows {
        let row = format!(
            "| `{}` | {} | {} |\n",
            addr_cell(addr),
            phase,
            fmt_duration(*elapsed)
        );
        if !b.push(&row) {
            break;
        }
    }
    b.push("\n</details>\n");
}

/// Render the **final** view — written once to `$GITHUB_STEP_SUMMARY`.
///
/// Not the live view at `t = end`: it reports what *happened* (durations, cache
/// outcome, what broke) rather than what is *happening* (currently-running
/// targets, which at close is by definition almost nothing).
pub(crate) fn render_final(t: &Tally, ctx: &RenderCtx<'_>, budget: usize) -> String {
    let mut b = Budgeted::new(budget);
    let c = t.counters();
    let elapsed = t.elapsed_ms(ctx.now_ms);
    let (_, total, _) = t.progress();

    if c.roots_total > 0 {
        b.push(&format!(
            "## ❌ {} — {} of {} targets failed in {}\n\n",
            ctx.heading,
            fmt_count(c.roots_total),
            fmt_count(total),
            fmt_duration(elapsed)
        ));
        let mut alert = format!(
            "> [!CAUTION]\n> **{} root failure{}**",
            fmt_count(c.roots_total),
            if c.roots_total == 1 { "" } else { "s" }
        );
        if c.blocked > 0 {
            alert.push_str(&format!(
                ", {} targets blocked downstream",
                fmt_count(c.blocked)
            ));
        }
        alert.push_str(".\n\n");
        b.push(&alert);
        push_final_failures(&mut b, t, &c);
    } else if c.executed == 0 && c.cached() > 0 {
        // All-cached: one line earns the whole report.
        b.push(&format!(
            "## ✅ {} — {} · {}/{} targets, nothing executed\n\n",
            ctx.heading,
            fmt_duration(elapsed),
            fmt_count(c.cached()),
            fmt_count(total)
        ));
        b.push(&format!(
            "{} cache hits ({} local, {} remote)\n",
            fmt_count(c.cached()),
            fmt_count(c.cached_local),
            fmt_count(c.cached_remote)
        ));
        return b.finish();
    } else {
        b.push(&format!(
            "## ✅ {} — {}\n\n",
            ctx.heading,
            fmt_duration(elapsed)
        ));
    }

    push_summary_table(&mut b, t, &c);
    push_zero_hit_diagnosis(&mut b, &c);
    push_slowest(&mut b, t);
    push_miss_rollup(&mut b, t, &c);

    if let Some(url) = ctx.run_url {
        b.push(&format!("\n[Full log]({url})\n"));
    }
    b.finish()
}

fn push_summary_table(b: &mut Budgeted, t: &Tally, c: &Counters) {
    let (done, total, _) = t.progress();
    let mut s = String::from("| | |\n| --- | --- |\n");
    s.push_str(&format!(
        "| targets | {} matched · {} ok · {} failed |\n",
        fmt_count(total),
        fmt_count(done.saturating_sub(c.roots_total.min(done))),
        fmt_count(c.roots_total)
    ));
    match c.hit_rate() {
        Some(rate) => s.push_str(&format!(
            "| cache | {} hits ({} local, {} remote) · **{:.1}% hit rate** · {} misses |\n",
            fmt_count(c.cached()),
            fmt_count(c.cached_local),
            fmt_count(c.cached_remote),
            rate * 100.0,
            fmt_count(c.misses())
        )),
        // Never render "0%" for a cache that was never consulted.
        None => s.push_str("| cache | not consulted |\n"),
    }
    s.push_str(&format!(
        "| executed | {} targets |\n",
        fmt_count(c.executed)
    ));
    if c.blocked > 0 {
        s.push_str(&format!(
            "| blocked | {} targets downstream of a failure |\n",
            fmt_count(c.blocked)
        ));
    }
    s.push('\n');
    b.push(&s);
}

/// The "why did I get no cache hits" sentence.
///
/// Deliberately prose naming the dominant cause rather than a number: `0 cached`
/// sends someone to Slack. The precise reason needs `MissReason` on the miss
/// events (`docs/GHA_REPORTING.md` §7.2); until then this says what *is* known
/// and points at the command that answers the rest.
fn push_zero_hit_diagnosis(b: &mut Budgeted, c: &Counters) {
    if c.cached() > 0 || c.misses() == 0 {
        return;
    }
    b.push(&format!(
        "\n> [!WARNING]\n> **0 of {} targets hit cache.** Every consulted target missed.\n\
         > Inspect one to see what changed: `heph inspect hashin <target>`\n\n",
        fmt_count(c.misses())
    ));
}

fn push_final_failures(b: &mut Budgeted, t: &Tally, c: &Counters) {
    for r in t.roots() {
        let mut s = format!("### `{}`\n\n", addr_cell(&r.addr));
        // A target that failed before it started executing has neither. Omit the
        // line rather than rendering a lone `unknown`.
        let mut meta: Vec<String> = Vec::new();
        if let Some(d) = r.driver.as_deref() {
            meta.push(format!("`{d}`"));
        }
        if let Some(d) = r.duration_ms {
            meta.push(format!("executed {}", fmt_duration(d)));
        }
        if let Some(status) = r.exit_status.as_deref() {
            meta.push(truncate_chars(status, 64));
        }
        if !meta.is_empty() {
            s.push_str(&format!("{}\n\n", meta.join(" · ")));
        }
        s.push_str(&format!("{}\n\n", one_line(&r.message, MAX_MSG_CHARS)));
        // The log tail is the whole product of a failure report — the message is
        // an index, this is what actually says what went wrong. Bounded in bytes:
        // one line of a minified bundle can be megabytes on its own.
        if let Some(tail) = r.log_tail.as_ref().and_then(final_log_tail) {
            s.push_str(&format!("```\n{tail}\n```\n\n"));
        }
        s.push_str(&format!("Reproduce: `heph run {}`\n", addr_cell(&r.addr)));
        if r.blocked > 0 {
            s.push_str(&format!(
                "\n**{} targets blocked downstream** — {}\n",
                fmt_count(r.blocked),
                pkg_rollup(r, 5)
            ));
        }
        s.push('\n');
        if !b.push(&s) {
            break;
        }
    }
    let shown = t.roots().len();
    if c.roots_total > shown {
        b.push(&format!(
            "…and {} more root failures.\n\n",
            fmt_count(c.roots_total.saturating_sub(shown))
        ));
    }
}

fn push_slowest(b: &mut Budgeted, t: &Tally) {
    let slowest = t.slowest();
    if slowest.is_empty() {
        return;
    }
    b.push(&format!(
        "<details><summary>Slowest {} executed targets</summary>\n\n\
         | target | driver | duration |\n| --- | --- | --- |\n",
        slowest.len().min(FINAL_SLOWEST_ROWS)
    ));
    for cmp in slowest.iter().take(FINAL_SLOWEST_ROWS) {
        let row = format!(
            "| `{}` | {} | {} |\n",
            addr_cell(&cmp.addr),
            cmp.driver.as_deref().unwrap_or("—"),
            fmt_duration(cmp.duration_ms)
        );
        if !b.push(&row) {
            break;
        }
    }
    b.push("\n</details>\n\n");
}

fn push_miss_rollup(b: &mut Budgeted, t: &Tally, c: &Counters) {
    let (rows, total_pkgs) = t.misses_by_package(FINAL_PKG_ROWS);
    if rows.is_empty() {
        return;
    }
    // At 20k targets a per-target miss list is unreadable, and the interesting
    // fact — that the misses are concentrated in a couple of packages — only
    // exists at the package level.
    b.push(&format!(
        "<details><summary>Cache misses by package ({} across {} packages)</summary>\n\n\
         | package | misses |\n| --- | --- |\n",
        fmt_count(c.misses()),
        fmt_count(total_pkgs)
    ));
    for (pkg, n) in &rows {
        let row = format!("| `{}` | {} |\n", addr_cell(pkg), fmt_count(*n));
        if !b.push(&row) {
            break;
        }
    }
    if total_pkgs > rows.len() {
        b.push(&format!(
            "\n_…and {} more packages._\n",
            fmt_count(total_pkgs.saturating_sub(rows.len()))
        ));
    }
    b.push("\n</details>\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::events::{BuildEvent, BuildEventKind};

    fn ev(at: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent {
            at_unix_ms: at,
            kind,
        }
    }

    fn ctx<'a>(heading: &'a str, now_ms: u64) -> RenderCtx<'a> {
        RenderCtx {
            heading,
            now_ms,
            slow_after_ms: 30_000,
            run_url: None,
        }
    }

    /// A build with `matched` top-level targets, `failing` root failures, and
    /// `blocked` collateral failures under the first root.
    fn build(matched: usize, failing: usize, blocked: usize) -> Tally {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::RequestConfig {
                max_workers: 64,
                fail_fast: false,
            },
        ));
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: (0..matched)
                    .map(|i| format!("//pkg{}:t{i}", i % 50))
                    .collect(),
                complete: true,
            },
        ));
        for i in 0..failing {
            t.apply(&ev(
                1_000,
                BuildEventKind::ResultEnd {
                    addr: format!("//root{i}:broken"),
                    error: Some("target failed".into()),
                    upstream_of: None,
                    exit_status: Some("exit status: 1".into()),
                    log_tail: None,
                },
            ));
        }
        for i in 0..blocked {
            t.apply(&ev(
                2_000,
                BuildEventKind::ResultEnd {
                    addr: format!("//services/api:t{i}"),
                    error: Some("dependency failed".into()),
                    upstream_of: Some("//root0:broken".into()),
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        t
    }

    #[test]
    fn live_view_at_20k_targets_stays_small() {
        // The whole point of the live view: it must be readable on a phone
        // regardless of graph size.
        let mut t = build(20_000, 0, 0);
        for i in 0..500 {
            t.apply(&ev(
                0,
                BuildEventKind::ExecuteStart {
                    addr: format!("//pkg{}:running{i}", i % 50),
                    driver: "exec".into(),
                    cache: false,
                },
            ));
        }
        let md = render_live(&t, &ctx("heph run //...", 600_000), COMMENT_LIMIT);
        assert!(
            md.len() < 2_000,
            "live view stays small: {} bytes",
            md.len()
        );
        assert!(md.contains("20,000"), "total shown with separators: {md}");
        assert!(
            md.contains("of 500 over 30s"),
            "honest count, sampled rows: {md}"
        );
        assert_eq!(
            md.matches("| `//pkg").count(),
            LIVE_RUNNING_ROWS,
            "running rows capped: {md}"
        );
    }

    #[test]
    fn live_view_puts_failures_above_the_counts() {
        let t = build(20_000, 2, 4_117);
        let md = render_live(&t, &ctx("heph run //...", 120_000), COMMENT_LIMIT);
        let alert = md.find("[!CAUTION]").expect("alert present");
        let counts = md.find("done ·").expect("counts present");
        assert!(alert < counts, "failures come first: {md}");
        assert!(md.contains("**2 failed** (+4,117 blocked)"), "{md}");
    }

    #[test]
    fn a_huge_collateral_cone_renders_no_per_target_rows() {
        // One broken leaf under 4,117 dependents. The old renderer produced a row
        // per failure; this must produce two root sections and a count.
        let t = build(20_000, 2, 4_117);
        let md = render_live(&t, &ctx("heph run //...", 120_000), COMMENT_LIMIT);
        assert!(
            !md.contains("dependency failed"),
            "collateral is never listed: {md}"
        );
        assert_eq!(md.matches("### `//root").count(), 2, "two roots: {md}");
        assert!(
            md.contains("**4,109 targets blocked**") || md.contains("4,117 blocked"),
            "collateral counted: {md}"
        );
        assert!(
            md.contains("`//services/api`"),
            "blocked rolled up by package: {md}"
        );
    }

    #[test]
    fn budget_is_never_exceeded_by_pathological_failures() {
        // 200 roots, each with a 4 KB single-line message — the case that froze
        // the old comment at a silent 422.
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        let huge = "x".repeat(4096);
        for i in 0..200 {
            t.apply(&ev(
                1_000,
                BuildEventKind::ResultEnd {
                    addr: format!("//pkg{i}:broken"),
                    error: Some(format!("execute failed: {huge}")),
                    upstream_of: None,
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        t.set_closed();

        for budget in [COMMENT_LIMIT, 4_096, 512] {
            let live = render_live(&t, &ctx("heph run //...", 9_000), budget);
            assert!(
                live.len() <= budget,
                "live over budget {budget}: {}",
                live.len()
            );
            let fin = render_final(&t, &ctx("heph run //...", 9_000), budget);
            assert!(
                fin.len() <= budget,
                "final over budget {budget}: {}",
                fin.len()
            );
        }
    }

    #[test]
    fn an_unbounded_message_is_capped_in_width() {
        let mut t = Tally::default();
        t.apply(&ev(
            1_000,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                // `format!("{e:#}")` puts the whole chain on ONE line, so a line
                // cap alone would not bound this.
                error: Some("y".repeat(10_000)),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        let md = render_live(&t, &ctx("heph run //...", 9_000), COMMENT_LIMIT);
        assert!(md.len() < 1_500, "width-capped: {} bytes", md.len());
        assert!(md.contains('…'), "truncation is visible: {md}");
    }

    #[test]
    fn multibyte_messages_and_addrs_do_not_panic() {
        let mut t = Tally::default();
        t.apply(&ev(
            1_000,
            BuildEventKind::ResultEnd {
                addr: format!("//パッケージ:{}", "名前".repeat(200)),
                error: Some("エラー".repeat(500)),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        let md = render_live(&t, &ctx("heph run //...", 9_000), COMMENT_LIMIT);
        assert!(md.contains("//パッケージ"), "rendered: {md}");
    }

    #[test]
    fn truncation_is_always_announced() {
        let t = build(20_000, 50, 10_000);
        let md = render_final(&t, &ctx("heph run //...", 120_000), 700);
        assert!(md.len() <= 700);
        assert!(
            md.contains("truncated"),
            "silent truncation is the same bug class as the silent 422: {md}"
        );
    }

    #[test]
    fn all_cached_final_is_one_line() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: (0..20_000).map(|i| format!("//pkg:t{i}")).collect(),
                complete: true,
            },
        ));
        for i in 0..20_000 {
            t.apply(&ev(
                1,
                BuildEventKind::LocalCacheHit {
                    addr: format!("//pkg:t{i}"),
                },
            ));
            t.apply(&ev(
                2,
                BuildEventKind::ResultEnd {
                    addr: format!("//pkg:t{i}"),
                    error: None,
                    upstream_of: None,
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        t.set_closed();
        let md = render_final(&t, &ctx("heph run //...", 41_000), SUMMARY_BUDGET);
        assert!(md.contains("nothing executed"), "{md}");
        assert!(md.contains("20,000 cache hits"), "{md}");
        assert!(md.len() < 300, "one line, not a wall: {} bytes", md.len());
    }

    #[test]
    fn final_view_reports_what_happened_not_what_is_running() {
        // The old renderer showed *currently running* targets in the final
        // summary, which at close is by definition almost empty.
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:slow".into()],
                complete: true,
            },
        ));
        t.apply(&ev(
            0,
            BuildEventKind::ExecuteStart {
                addr: "//a:slow".into(),
                driver: "exec".into(),
                cache: false,
            },
        ));
        t.apply(&ev(
            0,
            BuildEventKind::LocalCacheMiss {
                addr: "//a:slow".into(),
            },
        ));
        t.apply(&ev(
            252_000,
            BuildEventKind::ExecuteEnd {
                addr: "//a:slow".into(),
                error: None,
            },
        ));
        t.apply(&ev(
            252_000,
            BuildEventKind::ResultEnd {
                addr: "//a:slow".into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        t.set_closed();

        let md = render_final(&t, &ctx("heph run //...", 999_000), SUMMARY_BUDGET);
        assert!(md.contains("Slowest"), "durations reported: {md}");
        assert!(md.contains("4m12s"), "the actual duration: {md}");
        assert!(
            !md.contains("Running longest"),
            "no live-state section in the final view: {md}"
        );
        assert!(md.contains("Cache misses by package"), "rollup: {md}");
    }

    #[test]
    fn zero_hit_rate_is_diagnosed_not_just_reported() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        for i in 0..500 {
            t.apply(&ev(
                0,
                BuildEventKind::LocalCacheMiss {
                    addr: format!("//pkg:t{i}"),
                },
            ));
        }
        t.set_closed();
        let md = render_final(&t, &ctx("heph run //...", 9_000), SUMMARY_BUDGET);
        assert!(md.contains("0 of 500 targets hit cache"), "{md}");
        assert!(
            md.contains("heph inspect hashin"),
            "points at the next command: {md}"
        );
    }

    #[test]
    fn cache_never_consulted_does_not_render_zero_percent() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        t.apply(&ev(
            5,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        t.set_closed();
        let md = render_final(&t, &ctx("heph run //...", 9_000), SUMMARY_BUDGET);
        assert!(md.contains("not consulted"), "{md}");
        assert!(!md.contains("0.0% hit rate"), "0% would be a lie: {md}");
    }

    #[test]
    fn background_uploads_render_as_one_aggregate_line() {
        let mut t = build(20_000, 0, 0);
        for i in 0..5_000 {
            t.apply(&ev(
                0,
                BuildEventKind::RemoteCacheWriteStart {
                    addr: format!("//pkg:t{i}"),
                },
            ));
        }
        let md = render_live(&t, &ctx("heph run //...", 600_000), COMMENT_LIMIT);
        assert!(md.contains("↑ 5,000 cache uploads in flight"), "{md}");
        assert!(
            !md.contains("remote cache write |"),
            "never per-target rows: {md}"
        );
    }

    #[test]
    fn lock_waits_are_surfaced_with_their_holder() {
        let mut t = build(100, 0, 0);
        t.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//a:x".into(),
                holder_pid: Some(4412),
            },
        ));
        let md = render_live(&t, &ctx("heph run //...", 600_000), COMMENT_LIMIT);
        assert!(md.contains("waiting on the result lock"), "{md}");
        assert!(md.contains("pid 4412"), "{md}");
    }

    #[test]
    fn clock_is_quantized_so_suppression_can_fire() {
        // An unquantized clock changes on every render, so every tick would PATCH
        // even when nothing about the build changed.
        assert_eq!(fmt_clock(1_000 * (14 * 3600 + 2 * 60 + 11)), "14:02:10Z");
        assert_eq!(fmt_clock(1_000 * (14 * 3600 + 2 * 60 + 14)), "14:02:10Z");
        assert_eq!(fmt_clock(1_000 * (14 * 3600 + 2 * 60 + 15)), "14:02:15Z");
    }

    #[test]
    fn duration_and_count_formatting() {
        assert_eq!(fmt_duration(8_200), "8s");
        assert_eq!(fmt_duration(252_000), "4m12s");
        assert_eq!(fmt_duration(3_930_000), "1h05m");
        assert_eq!(fmt_count(4_117), "4,117");
        assert_eq!(fmt_count(20_140), "20,140");
        assert_eq!(fmt_count(7), "7");
    }

    #[test]
    fn fail_fast_mode_is_disclosed() {
        // Otherwise a one-failure report reads as "one thing is broken" when the
        // truth is "we stopped looking".
        //
        // The mode comes off the event stream, not from argv: the hook is handed
        // what it needs rather than discovering it, and an argv scan would
        // false-positive on a flag *value* like `--define X=--ff`.
        let mut t = build(20_000, 1, 0);
        t.apply(&ev(
            0,
            BuildEventKind::RequestConfig {
                max_workers: 64,
                fail_fast: true,
            },
        ));
        let md = render_live(&t, &ctx("heph run //...", 9_000), COMMENT_LIMIT);
        assert!(md.contains("Stopped at the first failure"), "{md}");

        // And absent by default, so a keep-going build is not mislabelled.
        let plain = build(20_000, 1, 0);
        let md = render_live(&plain, &ctx("heph run //...", 9_000), COMMENT_LIMIT);
        assert!(!md.contains("Stopped at the first failure"), "{md}");
    }

    /// Prints the rendered views for eyeballing against the mockups in
    /// `docs/GHA_REPORTING.md`. Ignored: it asserts nothing, it is a way to look
    /// at the output.
    ///
    /// `cargo test -p plugin-gha preview -- --ignored --nocapture`
    #[test]
    #[ignore = "prints output for human review; asserts nothing"]
    fn preview() {
        let mut t = build(20_000, 0, 0);
        for i in 0..218 {
            t.apply(&ev(
                i,
                BuildEventKind::ExecuteStart {
                    addr: format!("//services/api:image{i}"),
                    driver: "exec".into(),
                    cache: false,
                },
            ));
        }
        for i in 0..8_412 {
            t.apply(&ev(
                1,
                BuildEventKind::LocalCacheHit {
                    addr: format!("//pkg{}:t{i}", i % 50),
                },
            ));
            t.apply(&ev(
                2,
                BuildEventKind::ResultEnd {
                    addr: format!("//pkg{}:t{i}", i % 50),
                    error: None,
                    upstream_of: None,
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        t.apply(&ev(
            0,
            BuildEventKind::RequestConfig {
                max_workers: 64,
                fail_fast: false,
            },
        ));
        println!("\n===== LIVE, healthy =====\n");
        println!(
            "{}",
            render_live(&t, &ctx("run //...", 374_000), COMMENT_LIMIT)
        );

        let t2 = build(20_000, 2, 4_117);
        println!("\n===== LIVE, cone expanding =====\n");
        println!(
            "{}",
            render_live(&t2, &ctx("run //...", 182_000), COMMENT_LIMIT)
        );

        let mut t3 = build(20_000, 2, 4_117);
        t3.set_closed();
        println!("\n===== FINAL, failed =====\n");
        println!(
            "{}",
            render_final(&t3, &ctx("run //...", 468_000), SUMMARY_BUDGET)
        );
    }
}
