#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The approval gate end to end: a gated target's request names where each
//! notice's text is kept — artifacts in the local store, or a source file in
//! the workspace — and never carries or writes the text itself. Every reader,
//! the live view included, reads it from there through one capped reader
//! (`read_notice_from_store`). A `cache = False` input is spilled into the
//! durable store before the gate asks, so it reads like any other.

use async_trait::async_trait;
use hcore::approval_notice::{NoticeOrigin, NoticeRoots, NoticeSource};
use hcore::hasync::StdCancellationToken;
use heph::engine::approval::{
    ApprovalHandler, ApprovalRequest, read_notice_capped, read_notice_from_store,
};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

/// What the handler saw: the request, and each notice as it read it from the
/// store while the gate was pending.
struct Seen {
    req: ApprovalRequest,
    texts: Vec<Option<String>>,
}

/// Reads every notice while the gate waits on it — what the live view does —
/// then approves, keeping what it saw for the test.
struct Recording {
    cap: Option<usize>,
    seen: Mutex<Vec<Seen>>,
}

fn read_all(
    roots: &NoticeRoots,
    sources: &[Vec<NoticeSource>],
    cap: Option<usize>,
) -> Vec<Option<String>> {
    sources
        .iter()
        .map(|s| match cap {
            Some(cap) => read_notice_capped(roots, s, cap),
            None => read_notice_from_store(roots, s),
        })
        .collect()
}

#[async_trait]
impl ApprovalHandler for Recording {
    async fn request_approval(
        &self,
        req: ApprovalRequest,
        _ctoken: &StdCancellationToken,
    ) -> anyhow::Result<bool> {
        let roots = req.roots.clone();
        let sources: Vec<_> = req.notices.iter().map(|n| n.sources.clone()).collect();
        let cap = self.cap;
        let texts = tokio::task::spawn_blocking(move || read_all(&roots, &sources, cap)).await?;
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen { req, texts });
        Ok(true)
    }
}

fn engine(root: &Path) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config::for_tests(root.to_path_buf()))?;
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    let e = Arc::new(e);
    e.install_exec_runner_host();
    Ok(e)
}

/// `//p:push`, gated on a notice rendered from `//p:plan`, whose target
/// arguments are `plan_args`.
fn build_file(plan_args: &str) -> String {
    format!(
        r#"
target(
    name = "plan",
    driver = "bash",
    run = "printf 'push api:prod\n' > $OUT",
    out = "plan.txt",{plan_args}
)

target(
    name = "push",
    driver = "bash",
    deps = {{"plan": ":plan"}},
    run = "echo pushed > $OUT",
    out = "out.txt",
    approval = {{"required": True, "notice": ["plan"]}},
)
"#
    )
}

/// Run `//p:push` in a workspace with `build`, the handler reading notices
/// capped at `cap` (the default cap when `None`), and return the engine (still
/// alive, as a running build is while a viewer reads) and what the handler saw.
async fn gated_run(
    ws: &Path,
    build: &str,
    cap: Option<usize>,
) -> anyhow::Result<(Arc<Engine>, Seen)> {
    std::fs::create_dir_all(ws.join("p"))?;
    std::fs::write(ws.join("p/BUILD"), build)?;
    let engine = engine(ws)?;
    let handler = Arc::new(Recording {
        cap,
        seen: Mutex::new(Vec::new()),
    });
    let rs = engine.new_state_full(
        true,
        None,
        hcore::pending::Pending::new_counter(),
        Engine::DEFAULT_LOG_TAIL_LINES,
        Some(handler.clone()),
    );
    Arc::clone(&engine)
        .result_addr(
            rs,
            &parse_addr("//p:push")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    let mut seen =
        std::mem::take(&mut *handler.seen.lock().unwrap_or_else(PoisonError::into_inner));
    assert_eq!(seen.len(), 1, "one gate, one request");
    Ok((engine, seen.remove(0)))
}

/// The handler (the live view) reads the notice from the store while the
/// gate waits, and the gate writes no notice anywhere under heph's home.
#[tokio::test]
async fn the_live_view_reads_the_notice_from_the_store_and_nothing_is_written() -> anyhow::Result<()>
{
    let ws = tempfile::tempdir()?;
    let (engine, seen) = gated_run(ws.path(), &build_file(""), None).await?;

    let notice = &seen.req.notices[0];
    assert_eq!(notice.name, "plan");
    assert_eq!(seen.texts, [Some("push api:prod\n".to_owned())]);
    let approval_dir = engine.home.join("approval");
    assert!(
        !approval_dir.exists(),
        "no notice is written to disk: {:?}",
        std::fs::read_dir(&approval_dir).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );
    Ok(())
}

/// What an attached viewer does with the sources a session records: read the
/// notice again from the local cache, read-only, by the roots the request
/// names.
#[tokio::test]
async fn a_reader_reads_the_notice_from_the_store() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let (_engine, seen) = gated_run(ws.path(), &build_file(""), None).await?;

    let notice = &seen.req.notices[0];
    assert_eq!(notice.sources.len(), 1, "{:?}", notice.sources);
    assert_eq!(notice.sources[0].addr, "//p:plan");
    assert!(
        matches!(notice.sources[0].origin, NoticeOrigin::Cache { .. }),
        "{:?}",
        notice.sources[0].origin
    );
    let roots = seen.req.roots.clone();
    let sources = notice.sources.clone();
    let read =
        tokio::task::spawn_blocking(move || read_notice_from_store(&roots, &sources)).await?;
    assert_eq!(read.as_deref(), Some("push api:prod\n"));
    Ok(())
}

/// A `cache = False` input keeps its revision only in the run's memory; the
/// gate spills it into the durable store under its own `{hashin}_{nanos}`
/// key before asking, so the notice reads from the store like any other —
/// while the gate is pending, and to another reader after.
///
/// Red before the spill: this was `an_uncached_input_reads_as_unavailable`,
/// asserting `read_notice_from_store(..) == None` for the same input.
#[tokio::test]
async fn a_cache_false_input_is_readable_while_the_gate_is_pending() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let (_engine, seen) = gated_run(ws.path(), &build_file("\n    cache = False,"), None).await?;

    assert_eq!(
        seen.texts,
        [Some("push api:prod\n".to_owned())],
        "the gate's reader reads the spilled revision"
    );
    let notice = &seen.req.notices[0];
    let NoticeOrigin::Cache { revision, .. } = &notice.sources[0].origin else {
        panic!("not a cache origin: {:?}", notice.sources[0].origin);
    };
    assert!(
        revision.contains('_'),
        "an uncached input is spilled under a `{{hashin}}_{{nanos}}` key no lookup hits: {revision}"
    );
    let roots = seen.req.roots.clone();
    let sources = notice.sources.clone();
    let read =
        tokio::task::spawn_blocking(move || read_notice_from_store(&roots, &sources)).await?;
    assert_eq!(read.as_deref(), Some("push api:prod\n"));
    Ok(())
}

/// Past the cap a reader stops reading and ends the text with a line saying
/// how much there was and where to read all of it.
#[tokio::test]
async fn an_over_cap_notice_reads_truncated() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let (_engine, seen) = gated_run(ws.path(), &build_file(""), Some(4)).await?;

    assert_eq!(
        seen.texts,
        [Some(
            "push\n… notice truncated: 4 of 14 bytes shown; full text: heph inspect outputs //p:plan\n"
                .to_owned()
        )]
    );
    Ok(())
}

/// A notice whose input is a workspace source file is read in place, from the
/// workspace root — never copied into a cache — and only while the file still
/// hashes to what the gate saw.
#[tokio::test]
async fn a_source_file_notice_reads_from_the_workspace() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("p"))?;
    std::fs::write(ws.path().join("p/TERMS"), "terms v1\n")?;
    let build = r#"
target(
    name = "push",
    driver = "bash",
    deps = {"terms": "//@heph/fs:file@f=p/TERMS"},
    run = "echo pushed > $OUT",
    out = "out.txt",
    approval = {"required": True, "notice": ["terms"]},
)
"#;
    let (_engine, seen) = gated_run(ws.path(), build, None).await?;

    assert_eq!(seen.texts, [Some("terms v1\n".to_owned())]);
    let notice = &seen.req.notices[0];
    assert_eq!(notice.sources.len(), 1, "{:?}", notice.sources);
    let NoticeOrigin::File { path, .. } = &notice.sources[0].origin else {
        panic!("not a file origin: {:?}", notice.sources[0].origin);
    };
    assert_eq!(path, "p/TERMS", "recorded relative to the workspace root");

    // The file changed since the gate: what a reader would show is no
    // longer what was approved, so it reads as unavailable.
    std::fs::write(ws.path().join("p/TERMS"), "terms v2\n")?;
    let roots = seen.req.roots.clone();
    let sources = notice.sources.clone();
    let read =
        tokio::task::spawn_blocking(move || read_notice_from_store(&roots, &sources)).await?;
    assert_eq!(read, None, "a changed source file reads as unavailable");
    Ok(())
}
