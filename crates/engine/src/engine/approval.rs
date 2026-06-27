//! Approval gate: a target may declare `approval = True` / `approval = {required,
//! notice}` (see [`hplugin::provider::Approval`]). When `required`, the engine
//! pauses the target's execution and asks a host-supplied [`ApprovalHandler`] for
//! an explicit decision before running the driver. The handler is the seam
//! between the engine and the front-end (the interactive TUI, a stdin prompt, or
//! `--auto-approve`); the engine itself stays UI-agnostic.
//!
//! The notice — the rendered contents of the input groups named in
//! `approval.notice` — is computed here (the engine is the only place that can
//! resolve and read an input's artifacts) and handed to the handler ready to
//! display.

use crate::engine::Engine;
use crate::engine::link::LinkedTargetDef;
use crate::engine::provider::TargetSpec;
use crate::engine::request_state::RequestState;
use crate::engine::result::{OutputMatcher, ResultOptions};
use anyhow::Context;
use async_trait::async_trait;
use hcore::hartifactcontent::WalkEntryKind;
use hcore::hasync::StdCancellationToken;
use std::io::Read;
use std::sync::Arc;

/// One rendered notice shown to the user before they approve/reject: the input
/// group name and the concatenated text of every file that group resolves to.
#[derive(Debug, Clone)]
pub struct ApprovalNotice {
    pub name: String,
    pub content: String,
}

/// A pending approval handed to the [`ApprovalHandler`]: the target being gated
/// plus its (possibly empty) rendered notices.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub addr: String,
    pub notices: Vec<ApprovalNotice>,
}

/// Host-side decision maker for approval-gated targets. Implemented by the CLI
/// front-end (interactive TUI, stdin prompt, or auto-approve). Returning `false`
/// fails the target with [`crate::engine::error::ApprovalDeniedError`].
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    /// Resolve a single approval request. `ctoken` fires if the run is
    /// cancelled; the handler should stop waiting and return (typically `false`).
    async fn request_approval(
        &self,
        req: ApprovalRequest,
        ctoken: &StdCancellationToken,
    ) -> anyhow::Result<bool>;
}

impl Engine {
    /// Gate a target's execution on user approval. A no-op unless
    /// `spec.approval.required`. Renders the notice inputs, asks the request's
    /// [`ApprovalHandler`], and returns an [`ApprovalDeniedError`] if the user
    /// (or auto-approve policy) says no. Errors if a target requires approval but
    /// no handler is configured for the request — better to fail loudly than to
    /// silently run a gated target.
    ///
    /// [`ApprovalDeniedError`]: crate::engine::error::ApprovalDeniedError
    pub(crate) async fn gate_approval(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        spec: &TargetSpec,
        def: &LinkedTargetDef,
    ) -> anyhow::Result<()> {
        if !spec.approval.required {
            return Ok(());
        }
        let addr = &def.target.addr;
        let handler = rs.data.approval.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "target {} requires approval but no approval handler is configured \
                 (run interactively or pass --auto-approve)",
                addr.format()
            )
        })?;

        let mut notices = Vec::with_capacity(spec.approval.notice.len());
        for name in &spec.approval.notice {
            let content = self
                .clone()
                .render_notice(rs, def, name)
                .await
                .with_context(|| format!("rendering approval notice `{name}` for {}", addr.format()))?;
            notices.push(ApprovalNotice {
                name: name.clone(),
                content,
            });
        }

        let req = ApprovalRequest {
            addr: addr.format(),
            notices,
        };
        let approved = handler
            .request_approval(req, rs.ctoken())
            .await
            .with_context(|| format!("requesting approval for {}", addr.format()))?;
        if !approved {
            return Err(anyhow::Error::new(
                crate::engine::error::ApprovalDeniedError { addr: addr.clone() },
            ));
        }
        Ok(())
    }

    /// Resolve every input group whose `origin_id` is `name` and concatenate the
    /// UTF-8 text of their files. Binary bytes are rendered lossily. Errors if no
    /// input matches `name` — a notice naming a group that isn't a declared input
    /// is a build-file mistake, not something to silently skip.
    async fn render_notice(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        def: &LinkedTargetDef,
        name: &str,
    ) -> anyhow::Result<String> {
        // An input's `origin_id` is driver-encoded as `|`-delimited tokens with
        // the declared group name as one segment (e.g. `dep|plan|0` for the
        // `plan` dep group). Match the notice name against the whole id or any
        // segment so a notice can name a dep group directly.
        let inputs: Vec<_> = def
            .inputs
            .iter()
            .filter(|i| i.origin_id == name || i.origin_id.split('|').any(|seg| seg == name))
            .collect();
        if inputs.is_empty() {
            anyhow::bail!(
                "approval notice references `{name}`, which is not an input group of {}",
                def.target.addr.format()
            );
        }

        let mut out = String::new();
        for input in inputs {
            let res = self
                .clone()
                .result_addr(
                    rs.clone(),
                    &input.target.addr,
                    OutputMatcher::Exact(input.output_names.clone()),
                    &ResultOptions::default(),
                )
                .await
                .with_context(|| format!("resolving notice input {}", input.target.addr.format()))?;
            for art in &res.artifacts {
                for entry in art.walk()? {
                    let entry = entry?;
                    if let WalkEntryKind::File { mut data, .. } = entry.kind {
                        let mut bytes = Vec::new();
                        data.read_to_end(&mut bytes)
                            .with_context(|| "reading notice file")?;
                        out.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
            }
        }
        Ok(out)
    }
}
