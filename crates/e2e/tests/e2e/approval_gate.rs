#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The approval gate end to end: a gated target's notice is rendered from its
//! input group and stored under its content hash at gate time, which is how a
//! viewer reads it back (checked on read), while the legacy per-target file an
//! older attached viewer shows is still written.

use async_trait::async_trait;
use hcore::hasync::StdCancellationToken;
use heph::engine::approval::{ApprovalHandler, ApprovalRequest};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

/// Approves every request and keeps it for the test to read.
#[derive(Default)]
struct Recording(Mutex<Vec<ApprovalRequest>>);

#[async_trait]
impl ApprovalHandler for Recording {
    async fn request_approval(
        &self,
        req: ApprovalRequest,
        _ctoken: &StdCancellationToken,
    ) -> anyhow::Result<bool> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(req);
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

const BUILD: &str = r#"
target(
    name = "plan",
    driver = "bash",
    run = "printf 'push api:prod\n' > $OUT",
    out = "plan.txt",
)

target(
    name = "push",
    driver = "bash",
    deps = {"plan": ":plan"},
    run = "echo pushed > $OUT",
    out = "out.txt",
    approval = {"required": True, "notice": ["plan"]},
)
"#;

#[tokio::test]
async fn notice_is_stored_by_content_hash_at_gate_time() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("p"))?;
    std::fs::write(ws.path().join("p/BUILD"), BUILD)?;
    let engine = engine(ws.path())?;
    let handler = Arc::new(Recording::default());
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

    let requests = std::mem::take(&mut *handler.0.lock().unwrap_or_else(PoisonError::into_inner));
    assert_eq!(requests.len(), 1, "one gate, one request");
    let notice = &requests[0].notices[0];
    assert_eq!(notice.name, "plan");
    assert_eq!(notice.content, "push api:prod\n");
    assert_eq!(
        hcore::approval_notice::read_notice_blob(&engine.home, Some(&notice.hash)).as_deref(),
        Some("push api:prod\n"),
        "the notice reads back by its content hash"
    );
    assert_eq!(
        std::fs::read_to_string(&notice.path)?,
        "push api:prod\n",
        "the legacy file is still written for older viewers"
    );
    Ok(())
}
