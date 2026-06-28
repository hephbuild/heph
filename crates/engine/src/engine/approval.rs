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
use hmodel::htaddr::Addr;
use std::io::Read;
use std::sync::Arc;

/// One rendered notice shown to the user before they approve/reject: the input
/// group name, the concatenated text of every file that group resolves to, and
/// the on-disk path the full text was written to (so a front-end can offer a
/// clickable "open in editor" link rather than only an inline preview).
#[derive(Debug, Clone)]
pub struct ApprovalNotice {
    pub name: String,
    pub content: String,
    pub path: String,
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

/// Whether an input belongs to the notice group `name`. An input's `origin_id`
/// is driver-encoded as `|`-delimited tokens with the declared group name as one
/// segment (e.g. `dep|plan|0` for the `plan` dep group), so a notice can name the
/// whole id or any segment.
fn input_in_group(origin_id: &str, name: &str) -> bool {
    origin_id == name || origin_id.split('|').any(|seg| seg == name)
}

impl Engine {
    /// Validate that every `approval.notice` name resolves to at least one input
    /// group of the target. Called from `get_def_inner` once the def's input set
    /// is finalized — so a notice naming a group that does not exist fails at
    /// definition time, before any result resolution or execution, and the error
    /// is the same on every path (cache hit or miss). `origin_ids` are the
    /// inputs' driver-encoded ids (see [`input_in_group`]).
    pub(crate) fn validate_approval<'a>(
        spec: &TargetSpec,
        addr: &Addr,
        origin_ids: impl Iterator<Item = &'a str> + Clone,
    ) -> anyhow::Result<()> {
        if !spec.approval.required {
            return Ok(());
        }
        for name in &spec.approval.notice {
            if !origin_ids.clone().any(|oid| input_in_group(oid, name)) {
                anyhow::bail!(
                    "approval notice references `{name}`, which is not an input group of {}",
                    addr.format()
                );
            }
        }
        Ok(())
    }

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
                .with_context(|| {
                    format!("rendering approval notice `{name}` for {}", addr.format())
                })?;
            // Persist the full notice so the front-end can link to it (open in an
            // editor) instead of relying on a scrollable inline preview.
            let path = self
                .write_notice_file(addr, name, &content)
                .with_context(|| {
                    format!("writing approval notice `{name}` for {}", addr.format())
                })?;
            notices.push(ApprovalNotice {
                name: name.clone(),
                content,
                path,
            });
        }

        let req = ApprovalRequest {
            addr: addr.format(),
            notices,
        };
        // Counted before the prompt so a cancelled/errored gate still registers
        // as requested even when no decision lands.
        htelemetry::telemetry::record_approval_requested();
        let approved = handler
            .request_approval(req, rs.ctoken())
            .await
            .with_context(|| format!("requesting approval for {}", addr.format()))?;
        htelemetry::telemetry::record_approval_decision(approved);
        if !approved {
            return Err(anyhow::Error::new(
                crate::engine::error::ApprovalDeniedError { addr: addr.clone() },
            ));
        }
        Ok(())
    }

    /// Resolve every input group whose `origin_id` matches `name` (see
    /// [`input_in_group`]) and concatenate the UTF-8 text of their files. Binary
    /// bytes are rendered lossily. The notice name is validated up front by
    /// [`Self::validate_approval`]; an empty match here would mean the input set
    /// changed between link and execute, which is still surfaced as an error.
    async fn render_notice(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        def: &LinkedTargetDef,
        name: &str,
    ) -> anyhow::Result<String> {
        let inputs: Vec<_> = def
            .inputs
            .iter()
            .filter(|i| input_in_group(&i.origin_id, name))
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
                .with_context(|| {
                    format!("resolving notice input {}", input.target.addr.format())
                })?;
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

    /// Write a rendered notice under `<home>/approval/` and return its absolute
    /// path. The file name is derived from the target addr + group name so it is
    /// stable across runs (overwritten in place) and unique per (target, notice).
    fn write_notice_file(&self, addr: &Addr, name: &str, content: &str) -> anyhow::Result<String> {
        let dir = self.home.join("approval");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create approval dir {}", dir.display()))?;
        let sanitize = |s: &str| -> String {
            s.chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect()
        };
        let file = dir.join(format!(
            "{}__{}.txt",
            sanitize(&addr.format()),
            sanitize(name)
        ));
        std::fs::write(&file, content)
            .with_context(|| format!("write approval notice {}", file.display()))?;
        Ok(file.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::input_in_group;

    #[test]
    fn group_matches_origin_id_segment() {
        // Driver-encoded ids: the group name is one `|`-delimited segment.
        assert!(input_in_group("dep|plan|0", "plan"));
        assert!(input_in_group("tool|gosdk|2", "gosdk"));
        // Whole-id match (drivers that use the bare group as origin_id).
        assert!(input_in_group("plan", "plan"));
    }

    #[test]
    fn group_rejects_non_segment_match() {
        // A substring that is not a full segment must not match.
        assert!(!input_in_group("dep|planning|0", "plan"));
        assert!(!input_in_group("dep|plan|0", "dep|plan"));
        assert!(!input_in_group("dep|plan|0", "missing"));
    }
}
