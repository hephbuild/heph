//! Front-end implementations of the engine's [`ApprovalHandler`] for the `run`
//! command. The TUI handler surfaces the prompt on the live progress view and
//! awaits a `y`/`n` keypress; the non-TUI handler prints the notice to stderr
//! and either auto-approves or reads a decision from the controlling terminal.

use std::io::{Read, Write};

use anyhow::Context;
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
/// then auto-approve or prompt for `y`/`N` on the controlling terminal. Rejects
/// (with a clear error) when no terminal is available and `--auto-approve` was
/// not passed.
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
        print_notice(&req);
        if self.auto_approve {
            eprintln!("approval: auto-approved {} (--auto-approve)", req.addr);
            return Ok(true);
        }
        let addr = req.addr.clone();
        // The blocking tty read runs off the async worker.
        tokio::task::spawn_blocking(move || prompt_tty(&addr))
            .await
            .map_err(|e| anyhow::anyhow!("approval prompt task failed: {e}"))?
    }
}

/// Print the approval header and every notice's contents to stderr. Always runs
/// in non-TUI mode, regardless of `--auto-approve`, so a gated target's notice
/// is never silently skipped.
fn print_notice(req: &ApprovalRequest) {
    eprintln!();
    eprintln!("⚠ approval required: {}", req.addr);
    for notice in &req.notices {
        eprintln!("── notice: {} ──", notice.name);
        eprintln!("{}", notice.content);
    }
}

/// Prompt for a `y`/`N` decision on the controlling terminal. Reads `/dev/tty`
/// directly (not stdin, which may be a pipe). Returns `false` on anything but an
/// affirmative answer; errors when no terminal is available.
fn prompt_tty(addr: &str) -> anyhow::Result<bool> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| {
            anyhow::anyhow!(
                "approval required for {addr} but no terminal is available \
                 to prompt ({e}); pass --auto-approve to proceed non-interactively"
            )
        })?;
    write!(tty, "Approve {addr}? [y/N] ").context("write approval prompt")?;
    tty.flush().context("flush approval prompt")?;

    // Read a single line. One byte at a time so we stop at the newline without
    // consuming anything after it.
    let mut answer = String::new();
    let mut byte = [0u8; 1];
    loop {
        match tty.read(&mut byte) {
            Ok(0) => break, // EOF
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                answer.push(byte[0] as char);
            }
            Err(e) => return Err(anyhow::anyhow!("reading approval answer: {e}")),
        }
    }
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
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
}
