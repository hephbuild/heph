//! Front-end implementations of the engine's [`ApprovalHandler`] for the `run`
//! command. The TUI handler surfaces the prompt on the live progress view and
//! awaits a `y`/`n` keypress; the non-TUI handler prints the notice to stderr
//! and only proceeds when `--auto-approve` was passed (it never prompts).

use async_trait::async_trait;

use crate::engine::approval::{ApprovalHandler, ApprovalRequest, read_notice_from_store};
use crate::tui::{ApprovalBuild, ApprovalCenter, ApprovalNoticeView};
use hcore::approval_notice::NOTICE_UNAVAILABLE;
use hcore::hasync::{Cancellable, StdCancellationToken};

/// TUI-mode handler: register the gated target with the shared
/// [`ApprovalCenter`] (rendered on the live view) and await the user's decision.
/// `--auto-approve` short-circuits to yes without surfacing a prompt.
///
/// One handler serves one build's request (`Engine::new_state_full`),
/// so it holds that build's scope on the center: a standing Y/N covers this
/// build's requests only, and ends when the request (and so this handler) is
/// dropped.
pub struct TuiApprovalHandler {
    center: ApprovalCenter,
    build: ApprovalBuild,
    auto_approve: bool,
}

impl TuiApprovalHandler {
    pub fn new(center: ApprovalCenter, auto_approve: bool) -> Self {
        Self {
            build: center.build(),
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
        let texts = notice_texts(&req).await;
        let notices = req
            .notices
            .into_iter()
            .zip(texts)
            .map(|(n, content)| ApprovalNoticeView {
                name: n.name,
                content,
                sources: n.sources,
            })
            .collect();
        // Under a standing Y/N the receiver comes back already resolved.
        let rx = self.center.request(&self.build, req.addr, notices).rx;
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
        log_notice(&req).await;
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

/// Log every notice's contents, read from the store. Always runs in non-TUI
/// mode, regardless of `--auto-approve`, so a gated target's notice is never
/// silently skipped. The target addr is carried by the auto-approved /
/// requires-approval line, so no separate header is logged here.
async fn log_notice(req: &ApprovalRequest) {
    for (notice, text) in req.notices.iter().zip(notice_texts(req).await) {
        tracing::info!("notice {}:\n{}", notice.name, text);
    }
}

/// Each notice's text, in order, read from the store on the blocking pool
/// through the one capped reader every viewer uses
/// ([`read_notice_from_store`]); one that cannot be read is
/// [`NOTICE_UNAVAILABLE`].
async fn notice_texts(req: &ApprovalRequest) -> Vec<String> {
    if req.notices.is_empty() {
        return Vec::new();
    }
    let roots = req.roots.clone();
    let sources: Vec<_> = req.notices.iter().map(|n| n.sources.clone()).collect();
    hcore::blocking::run(move || {
        sources
            .iter()
            .map(|s| {
                read_notice_from_store(&roots, s).unwrap_or_else(|| NOTICE_UNAVAILABLE.to_owned())
            })
            .collect()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::approval::{ApprovalNotice, ApprovalRequest};
    use hcore::approval_notice::{NoticeOrigin, NoticeRoots, NoticeSource};
    use std::path::PathBuf;

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            addr: "//a:b".to_string(),
            notices: vec![],
            roots: NoticeRoots {
                home: PathBuf::from("/nonexistent-heph-home"),
                root: PathBuf::from("/nonexistent-heph-root"),
            },
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

    /// The live view shows what the store reader returns: a notice whose
    /// source is not in the store — or that names none — reads as
    /// "notice unavailable" on the prompt rather than as an empty notice.
    #[tokio::test]
    async fn tui_shows_an_unreadable_notice_as_unavailable() {
        let center = ApprovalCenter::new();
        let handler = TuiApprovalHandler::new(center.clone(), false);
        let ctoken = StdCancellationToken::new();
        let mut r = req();
        r.notices = vec![
            ApprovalNotice {
                name: "plan".into(),
                sources: vec![NoticeSource {
                    addr: "//a:plan".into(),
                    origin: NoticeOrigin::Cache {
                        addr: "//a:plan".into(),
                        revision: "h1".into(),
                        hashout: "o1".into(),
                    },
                }],
            },
            ApprovalNotice {
                name: "none".into(),
                sources: Vec::new(),
            },
        ];
        let (decided, ()) = tokio::join!(handler.request_approval(r, &ctoken), async {
            loop {
                if let Some(view) = center.current() {
                    let shown: Vec<_> = view
                        .notices
                        .iter()
                        .map(|n| (n.name.as_str(), n.content.as_str()))
                        .collect();
                    assert_eq!(
                        shown,
                        [("plan", NOTICE_UNAVAILABLE), ("none", NOTICE_UNAVAILABLE)]
                    );
                    center.respond(view.seq, false, Vec::new());
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        assert!(!decided.unwrap());
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
                if let Some(view) = center.current() {
                    center.respond(view.seq, true, Vec::new());
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
