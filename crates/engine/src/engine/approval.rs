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

/// [`read_notice_text`] on the blocking pool.
///
/// Rendering a notice walks every file of every matched artifact and pulls its
/// bytes out of the cache, which additionally parks the calling thread on any
/// queued sqlite write to those keys. The artifacts are `Arc`s, so the job owns
/// them outright and survives a dropped caller future.
///
/// A named wrapper rather than an inline `hcore::blocking::run`, so the
/// "off the worker" property has something a test can hold — see
/// `notice_rendering_runs_off_the_runtime_workers`.
async fn read_notice_text_blocking(
    artifacts: Vec<Arc<dyn hcore::hartifactcontent::Content>>,
) -> anyhow::Result<String> {
    hcore::blocking::run(move || read_notice_text(&artifacts)).await
}

/// Concatenate the UTF-8 text of every file in `artifacts`. Binary bytes are
/// rendered lossily — a notice is shown to a human, not parsed.
///
/// Synchronous and byte-moving: called only through
/// [`read_notice_text_blocking`].
fn read_notice_text(
    artifacts: &[Arc<dyn hcore::hartifactcontent::Content>],
) -> anyhow::Result<String> {
    let mut out = String::new();
    for art in artifacts {
        for entry in art.walk().context("walk notice artifact")? {
            let entry = entry.context("read notice artifact entry")?;
            if let WalkEntryKind::File { mut data, .. } = entry.kind {
                let mut bytes = Vec::new();
                data.read_to_end(&mut bytes)
                    .context("reading notice file")?;
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    Ok(out)
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
            let addr = input.target.addr.format();
            let text = read_notice_text_blocking(res.artifacts.clone())
                .await
                .with_context(|| format!("reading notice input {addr}"))?;
            out.push_str(&text);
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
    use super::{input_in_group, read_notice_text, read_notice_text_blocking};
    use hcore::hartifactcontent::{Content, WalkEntry};
    use std::sync::Arc;

    /// A tar of `(path, bytes)` pairs, as a `Content`.
    struct TarBytes(Vec<u8>);

    impl Content for TarBytes {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            Ok(Box::new(std::io::Cursor::new(self.0.clone())))
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(hcore::hartifactcontent::tar::TarWalker::new(
                std::io::Cursor::new(self.0.clone()),
            )?))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn tar_of(files: &[(&str, &[u8])]) -> Arc<dyn Content> {
        let mut packer = hcore::hartifactcontent::tar::TarPacker::new();
        for (rel, body) in files {
            packer.create_raw(body.to_vec(), *rel, false);
        }
        let mut bytes = Vec::new();
        packer.pack(&mut bytes).expect("pack");
        Arc::new(TarBytes(bytes))
    }

    /// The notice is the text a human reads before approving a target, so it
    /// concatenates every file of every matched artifact in order, and never
    /// fails on bytes that are not UTF-8 — a notice that errored out would block
    /// the approval rather than merely look odd.
    /// Rendering a notice must not run on a tokio worker: it walks every file of
    /// every matched artifact and reads its bytes out of the cache, and that read
    /// parks the calling thread on any queued sqlite write to those keys.
    ///
    /// The content records the thread it was walked on; `hcore::blocking`'s pool
    /// threads are the only ones named `heph-blocking-*`. Asserted positively —
    /// `!= "tokio-runtime-worker"` would pass vacuously, since `#[tokio::test]`
    /// runs the body on the test thread.
    #[tokio::test]
    async fn notice_rendering_runs_off_the_runtime_workers() {
        struct ThreadRecordingTar {
            inner: TarBytes,
            thread: Arc<std::sync::Mutex<Option<String>>>,
        }

        impl Content for ThreadRecordingTar {
            fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
                self.inner.reader()
            }
            fn walk(
                &self,
            ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>>
            {
                let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
                *slot = std::thread::current()
                    .name()
                    .map(std::borrow::ToOwned::to_owned);
                drop(slot);
                self.inner.walk()
            }
            fn hashout(&self) -> anyhow::Result<String> {
                self.inner.hashout()
            }
        }

        let mut packer = hcore::hartifactcontent::tar::TarPacker::new();
        packer.create_raw(b"notice body\n".to_vec(), "NOTICE.txt", false);
        let mut bytes = Vec::new();
        packer.pack(&mut bytes).expect("pack");
        let thread = Arc::new(std::sync::Mutex::new(None));
        let artifacts: Vec<Arc<dyn Content>> = vec![Arc::new(ThreadRecordingTar {
            inner: TarBytes(bytes),
            thread: Arc::clone(&thread),
        })];

        let out = read_notice_text_blocking(artifacts).await.expect("render");

        assert_eq!(out, "notice body\n", "the notice must still be rendered");
        let ran_on = thread.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            ran_on
                .as_deref()
                .is_some_and(|n| n.starts_with("heph-blocking")),
            "notice rendering must run on the blocking pool, ran on {ran_on:?}"
        );
    }

    #[test]
    fn notice_text_concatenates_every_file_and_survives_binary_bytes() {
        let artifacts = vec![
            tar_of(&[("a.txt", b"first\n"), ("b.txt", b"second\n")]),
            tar_of(&[("c.bin", &[0xff, 0xfe])]),
        ];

        let out = read_notice_text(&artifacts).expect("render");

        assert!(out.starts_with("first\nsecond\n"), "got {out:?}");
        assert!(
            out.contains('\u{fffd}'),
            "non-UTF-8 bytes must render lossily rather than fail: {out:?}"
        );
    }

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
