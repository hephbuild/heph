//! Front-end implementations of the engine's [`ApprovalHandler`] for the `run`
//! command. The TUI handler surfaces the prompt on the live progress view and
//! awaits a `y`/`n` keypress; the non-TUI handler prints the notice to stderr
//! and only proceeds when `--auto-approve` was passed (it never prompts).

use async_trait::async_trait;

use crate::engine::approval::{ApprovalHandler, ApprovalRequest};
use crate::tui::{ApprovalCenter, ApprovalNoticeView};
use hcore::hasync::{Cancellable, StdCancellationToken};

/// TUI-mode handler: register the gated target with the shared
/// [`ApprovalCenter`] (rendered on the live view) and await the user's decision.
/// `--auto-approve` short-circuits to yes without surfacing a prompt.
pub struct TuiApprovalHandler {
    center: ApprovalCenter,
    auto_approve: bool,
}

impl TuiApprovalHandler {
    pub fn new(center: ApprovalCenter, auto_approve: bool) -> Self {
        Self {
            center,
            auto_approve,
        }
    }
}

#[async_trait]
impl ApprovalHandler for TuiApprovalHandler {
    async fn request_approval(
        &self,
        req: ApprovalRequest,
        ctoken: &StdCancellationToken,
    ) -> anyhow::Result<bool> {
        if self.auto_approve {
            return Ok(true);
        }
        let notices = req
            .notices
            .into_iter()
            .map(|n| ApprovalNoticeView {
                name: n.name,
                content: n.content,
                path: n.path,
            })
            .collect();
        let rx = self.center.request(req.addr, notices);
        tokio::select! {
            // Sender dropped without a decision (shutdown) → treat as rejection.
            decided = rx => Ok(decided.unwrap_or(false)),
            // Run cancelled (Ctrl-C): stop waiting and reject.
            () = ctoken.cancelled() => Ok(false),
        }
    }
}

/// Non-TUI (CI / piped / `--no-tui`) handler: always print the notice to stderr,
/// then let the target through only when `--auto-approve` was passed. It never
/// prompts — there is no interactive view to host a decision — so without the
/// flag the gated target is rejected.
pub struct CliApprovalHandler {
    auto_approve: bool,
}

impl CliApprovalHandler {
    pub fn new(auto_approve: bool) -> Self {
        Self { auto_approve }
    }
}

#[async_trait]
impl ApprovalHandler for CliApprovalHandler {
    async fn request_approval(
        &self,
        req: ApprovalRequest,
        _ctoken: &StdCancellationToken,
    ) -> anyhow::Result<bool> {
        log_notice(&req);
        if self.auto_approve {
            tracing::info!("approval: auto-approved {} (--auto-approve)", req.addr);
            Ok(true)
        } else {
            tracing::warn!(
                "approval: {} requires approval; pass --auto-approve to proceed",
                req.addr
            );
            Ok(false)
        }
    }
}

/// Log every notice's contents. Always runs in non-TUI mode, regardless of
/// `--auto-approve`, so a gated target's notice is never silently skipped. The
/// target addr is carried by the auto-approved / requires-approval line, so no
/// separate header is logged here.
fn log_notice(req: &ApprovalRequest) {
    for notice in &req.notices {
        tracing::info!(
            "notice {} ({}):\n{}",
            notice.name,
            notice.path,
            notice.content
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::approval::ApprovalRequest;

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            addr: "//a:b".to_string(),
            notices: vec![],
        }
    }

    #[tokio::test]
    async fn tui_auto_approve_skips_prompt() {
        let center = ApprovalCenter::new();
        let handler = TuiApprovalHandler::new(center.clone(), true);
        let ctoken = StdCancellationToken::new();
        assert!(handler.request_approval(req(), &ctoken).await.unwrap());
        // Auto-approve never enqueues a prompt.
        assert!(!center.is_active());
    }

    #[tokio::test]
    async fn tui_resolves_from_center_decision() {
        let center = ApprovalCenter::new();
        let handler = TuiApprovalHandler::new(center.clone(), false);
        let ctoken = StdCancellationToken::new();
        // Drive the handler and the responder concurrently: the handler enqueues
        // and parks on the receiver; the responder waits for the prompt to appear,
        // then approves it.
        let (decided, ()) = tokio::join!(handler.request_approval(req(), &ctoken), async {
            loop {
                if center.is_active() {
                    center.respond(true);
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        assert!(decided.unwrap());
        assert!(!center.is_active());
    }

    #[tokio::test]
    async fn tui_cancellation_rejects() {
        let center = ApprovalCenter::new();
        let handler = TuiApprovalHandler::new(center.clone(), false);
        let ctoken = StdCancellationToken::new();
        ctoken.cancel();
        // A cancelled run rejects the gated target instead of hanging.
        assert!(!handler.request_approval(req(), &ctoken).await.unwrap());
    }

    #[tokio::test]
    async fn cli_auto_approve_returns_true() {
        let handler = CliApprovalHandler::new(true);
        let ctoken = StdCancellationToken::new();
        assert!(handler.request_approval(req(), &ctoken).await.unwrap());
    }

    #[tokio::test]
    async fn cli_without_auto_approve_rejects() {
        // Non-TUI never prompts: without --auto-approve the gated target is denied.
        let handler = CliApprovalHandler::new(false);
        let ctoken = StdCancellationToken::new();
        assert!(!handler.request_approval(req(), &ctoken).await.unwrap());
    }
}
