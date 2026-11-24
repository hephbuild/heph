//! Approval gate: a target may declare `approval = True` / `approval = {required,
//! notice}` (see [`hplugin::provider::Approval`]). When `required`, the engine
//! pauses the target's execution and asks a host-supplied [`ApprovalHandler`] for
//! an explicit decision before running the driver. The handler is the seam
//! between the engine and the front-end (the interactive TUI, a stdin prompt, or
//! `--auto-approve`); the engine itself stays UI-agnostic.
//!
//! The notice — the contents of the input groups named in `approval.notice` —
//! is never rendered here, written anywhere, or held in memory. At gate time
//! the engine resolves those inputs (it is the only place that can) and hands
//! the handler where each artifact's bytes are kept: a [`NoticeSource`] per
//! artifact. An artifact that lived only in this process's memory — the tmp
//! tier's copy of a `cache = False` input — is first spilled into the durable
//! local cache under its own `{hashin}_{nanos}` key. Every reader, the live
//! view and another process alike, then renders the text from the store with
//! [`read_notice_from_store`], capped (see [`hcore::approval_notice`]).
//! Diagnostics only: none of it reaches a hash, a sandbox, an env or an argv.

use crate::engine::Engine;
use crate::engine::link::LinkedTargetDef;
use crate::engine::local_cache::{CacheArtifact, LocalCache, MANIFEST_V2, ManifestArtifactType};
use crate::engine::provider::TargetSpec;
use crate::engine::request_state::RequestState;
use crate::engine::result::{OutputMatcher, PassthroughContent, ResultOptions};
use anyhow::Context;
use async_trait::async_trait;
use hcore::approval_notice::{NOTICE_READ_CAP, NoticeOrigin, NoticeRoots, NoticeSource};
use hcore::hartifactcontent::{Content, Origin, WalkEntryKind};
use hcore::hasync::StdCancellationToken;
use hmodel::htaddr::{Addr, parse_addr};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// One notice shown to the user before they approve/reject: the input group
/// name and where its text is kept, artifact by artifact, in order. Never the
/// text: a reader renders it with [`read_notice_from_store`].
#[derive(Debug, Clone)]
pub struct ApprovalNotice {
    pub name: String,
    pub sources: Vec<NoticeSource>,
}

/// A pending approval handed to the [`ApprovalHandler`]: the target being gated,
/// its (possibly empty) notices, and where their sources are read from.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub addr: String,
    pub notices: Vec<ApprovalNotice>,
    pub roots: NoticeRoots,
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
    /// `spec.approval.required`. Collects the notice inputs' sources, asks the
    /// request's [`ApprovalHandler`], and returns an [`ApprovalDeniedError`] if
    /// the user (or auto-approve policy) says no. Errors if a target requires
    /// approval but no handler is configured for the request — better to fail
    /// loudly than to silently run a gated target.
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
            let sources = self
                .clone()
                .notice_sources(rs, def, name)
                .await
                .with_context(|| {
                    format!("collecting approval notice `{name}` for {}", addr.format())
                })?;
            notices.push(ApprovalNotice {
                name: name.clone(),
                sources,
            });
        }

        let req = ApprovalRequest {
            addr: addr.format(),
            notices,
            roots: NoticeRoots {
                home: self.home.to_path_buf(),
                root: self.root().to_path_buf(),
            },
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
    /// [`input_in_group`]) and return where each of their artifacts is kept,
    /// in order. The notice name is validated up front by
    /// [`Self::validate_approval`]; an empty match here would mean the input
    /// set changed between link and execute, which is still surfaced as an
    /// error.
    ///
    /// Every source is readable from the store once this returns: a revision
    /// the tmp tier held in memory is spilled into the durable cache, and the
    /// manifest of every cache revision named is waited out of the write
    /// queue and checked present; a source file is checked present in the
    /// workspace. An artifact with no nameable origin is an error, not a
    /// silently unreadable notice.
    async fn notice_sources(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        def: &LinkedTargetDef,
        name: &str,
    ) -> anyhow::Result<Vec<NoticeSource>> {
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

        let mut sources = Vec::new();
        for input in inputs {
            let addr = &input.target.addr;
            let res = self
                .clone()
                .result_addr(
                    rs.clone(),
                    addr,
                    OutputMatcher::Exact(input.output_names.clone()),
                    &ResultOptions::default(),
                )
                .await
                .with_context(|| format!("resolving notice input {}", addr.format()))?;
            for artifact in &res.artifacts {
                let origin = artifact.origin().with_context(|| {
                    format!(
                        "notice input {} has an artifact no reader can find again",
                        addr.format()
                    )
                })?;
                self.persist_origin(&origin)
                    .await
                    .with_context(|| format!("storing notice input {}", addr.format()))?;
                sources.push(NoticeSource {
                    addr: addr.format(),
                    origin: self.notice_origin(origin)?,
                });
            }
        }
        Ok(sources)
    }

    /// Make `origin` readable by another process: spill a tmp revision it
    /// names into the durable cache, then wait its manifest out of the write
    /// queue. A source file is the workspace's own and is only checked
    /// present.
    async fn persist_origin(&self, origin: &Origin) -> anyhow::Result<()> {
        match origin {
            Origin::Cache { addr, revision, .. } => {
                let addr = parse_addr(addr).with_context(|| format!("parsing {addr}"))?;
                let tmp = Arc::clone(&self.local_cache_tmp);
                let (a, r) = (addr.clone(), revision.clone());
                // Off the workers: it moves bytes into the durable cache.
                hcore::blocking::run(move || tmp.persist_revision(&a, &r)).await?;
                // The manifest is a revision's last write, so once it is
                // committed so are its blobs.
                if !self.exists_local(&addr, revision, MANIFEST_V2).await? {
                    anyhow::bail!("revision {revision} of {addr} is not in the local cache");
                }
                Ok(())
            }
            Origin::File { path, .. } => {
                if !path.is_file() {
                    anyhow::bail!("source file {} is gone", path.display());
                }
                Ok(())
            }
            Origin::View { source, .. } => Box::pin(self.persist_origin(source)).await,
        }
    }

    /// `origin` as a session records it: a source file relative to the
    /// workspace root, the rest as they are.
    fn notice_origin(&self, origin: Origin) -> anyhow::Result<NoticeOrigin> {
        Ok(match origin {
            Origin::Cache {
                addr,
                revision,
                hashout,
            } => NoticeOrigin::Cache {
                addr,
                revision,
                hashout,
            },
            Origin::File {
                path,
                out_path,
                x,
                hashout,
            } => {
                let rel = path.strip_prefix(self.root()).with_context(|| {
                    format!("source file {} is outside the workspace", path.display())
                })?;
                NoticeOrigin::File {
                    path: rel.to_string_lossy().into_owned(),
                    out_path,
                    x,
                    hashout,
                }
            }
            Origin::View { source, paths } => NoticeOrigin::View {
                source: Box::new(self.notice_origin(*source)?),
                paths: paths
                    .into_iter()
                    .map(|(s, d)| {
                        (
                            s.to_string_lossy().into_owned(),
                            d.to_string_lossy().into_owned(),
                        )
                    })
                    .collect(),
            },
        })
    }
}

/// Render a notice from its [`NoticeSource`]s, read-only: no engine, no
/// build, no fetch, no write — what every reader does, the live view and an
/// attached viewer in another process alike. Capped at [`NOTICE_READ_CAP`]
/// bytes (see [`read_notice_capped`]). Matches
/// [`hcore::approval_notice::ReadNotice`].
pub fn read_notice_from_store(roots: &NoticeRoots, sources: &[NoticeSource]) -> Option<String> {
    read_notice_capped(roots, sources, NOTICE_READ_CAP)
}

/// [`read_notice_from_store`] with a cap of `cap` bytes. The files of the
/// sources are concatenated in order, rendered lossily (a notice is shown to a
/// human, not parsed); past `cap` bytes no file is read further, and the text
/// ends with a line naming the total size and the input to
/// `heph inspect outputs` for the rest.
///
/// `None` — show [`hcore::approval_notice::NOTICE_UNAVAILABLE`] — when there
/// are no sources, or any of them is not where it was recorded: a revision
/// gc'd since, a hashout the revision no longer holds, a source file that is
/// gone or (read through to its end) no longer hashes to what was recorded.
/// Blocking: call it off the render task.
pub fn read_notice_capped(
    roots: &NoticeRoots,
    sources: &[NoticeSource],
    cap: usize,
) -> Option<String> {
    if sources.is_empty() {
        return None;
    }
    let mut store = Store { roots, cache: None };
    let mut text = CappedText::new(cap);
    let mut read = || -> anyhow::Result<bool> {
        for source in sources {
            let found = store.each_file(&source.origin, &mut |_, data, size| {
                text.file(data, size, &source.addr)
            })?;
            if !found {
                return Ok(false);
            }
        }
        Ok(true)
    };
    match read() {
        Ok(true) => Some(text.finish()),
        Ok(false) => None,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "approval notice unavailable");
            None
        }
    }
}

/// Where one read of a notice finds its sources: the roots, and the local
/// cache, opened on first use.
struct Store<'a> {
    roots: &'a NoticeRoots,
    cache: Option<Arc<dyn LocalCache>>,
}

/// Called with a file's path, its bytes (unread) and its size.
type EachFile<'f> = dyn FnMut(&Path, Box<dyn Read>, u64) -> anyhow::Result<()> + 'f;

impl Store<'_> {
    /// Call `f` with each file `origin` names, in order: its path (what a
    /// view maps), its bytes (unread) and its size. `Ok(false)` when it is not
    /// where it was recorded.
    fn each_file(&mut self, origin: &NoticeOrigin, f: &mut EachFile<'_>) -> anyhow::Result<bool> {
        if let NoticeOrigin::View { source, paths } = origin {
            let paths: std::collections::HashMap<&Path, &Path> = paths
                .iter()
                .map(|(s, d)| (Path::new(s.as_str()), Path::new(d.as_str())))
                .collect();
            return self.each_file(source, &mut |path, data, size| match paths.get(path) {
                Some(dst) => f(dst, data, size),
                None => Ok(()),
            });
        }
        let Some(content) = self.open(origin)? else {
            return Ok(false);
        };
        for entry in content.walk().context("walk notice artifact")? {
            let entry = entry.context("read notice artifact entry")?;
            if let WalkEntryKind::File { data, size, .. } = entry.kind {
                f(&entry.path, data, size)?;
            }
        }
        Ok(true)
    }

    /// The content a cache entry or a source file names, or `None` when it
    /// is not there.
    fn open(&mut self, origin: &NoticeOrigin) -> anyhow::Result<Option<Arc<dyn Content>>> {
        Ok(match origin {
            NoticeOrigin::Cache {
                addr,
                revision,
                hashout,
            } => {
                let addr = parse_addr(addr).with_context(|| format!("parsing {addr}"))?;
                let Some(cache) = self.cache()? else {
                    return Ok(None);
                };
                let Some(manifest) = Engine::read_manifest_from(&cache, &addr, revision)? else {
                    return Ok(None);
                };
                manifest
                    .artifacts
                    .iter()
                    .find(|a| a.r#type == ManifestArtifactType::Output && &a.hashout == hashout)
                    .map(|a| -> Arc<dyn Content> {
                        Arc::new(CacheArtifact::from_manifest(&addr, revision, a, cache))
                    })
            }
            NoticeOrigin::File {
                path,
                out_path,
                x,
                hashout,
            } => {
                let path = self.roots.root.join(path);
                if !path.is_file() {
                    return Ok(None);
                }
                Some(Arc::new(PassthroughContent::new(
                    path.to_string_lossy().into_owned(),
                    out_path.clone(),
                    *x,
                    hashout.clone(),
                )))
            }
            // Walked through its source by `each_file`.
            NoticeOrigin::View { .. } => None,
        })
    }

    /// The local cache under `home`, read-only, opened once. `None` when the
    /// home has no cache: a read-only handle never creates the database, so
    /// that is checked first and answered at once rather than through the
    /// pool's open retry.
    fn cache(&mut self) -> anyhow::Result<Option<Arc<dyn LocalCache>>> {
        if let Some(cache) = &self.cache {
            return Ok(Some(Arc::clone(cache)));
        }
        let home = &self.roots.home;
        let db = crate::engine::local_cache::cache_db_path(home);
        if !db.exists() {
            return Ok(None);
        }
        let sqlite = Arc::new(
            crate::engine::local_cache_sqlite::LocalCacheSQLite::open_read_only(db)
                .context("opening the local cache read-only")?,
        );
        let blobs = Arc::new(crate::engine::local_cache_fs::LocalCacheFS::new(
            crate::engine::local_cache::cache_blobs_dir(home),
        )?);
        // The threshold only routes writes, and this handle makes none.
        let cache: Arc<dyn LocalCache> = Arc::new(
            crate::engine::local_cache_spill::LocalCacheSpill::new(sqlite, blobs, usize::MAX),
        );
        self.cache = Some(Arc::clone(&cache));
        Ok(Some(cache))
    }
}

/// A notice's text, up to `cap` bytes: each file rendered lossily, and past
/// the cap only counted, never read.
struct CappedText {
    out: String,
    cap: usize,
    /// Bytes read into `out` so far (before lossy rendering).
    read: u64,
    /// Bytes of every file seen, read or not.
    total: u64,
    /// The input whose file the cap fell in.
    truncated_in: Option<String>,
}

impl CappedText {
    fn new(cap: usize) -> Self {
        Self {
            out: String::new(),
            cap,
            read: 0,
            total: 0,
            truncated_in: None,
        }
    }

    fn file(&mut self, data: Box<dyn Read>, size: u64, addr: &str) -> anyhow::Result<()> {
        self.total = self.total.saturating_add(size);
        let room = (self.cap as u64).saturating_sub(self.read);
        if room > 0 {
            let mut bytes = Vec::new();
            data.take(room)
                .read_to_end(&mut bytes)
                .context("reading notice file")?;
            self.read = self.read.saturating_add(bytes.len() as u64);
            self.out.push_str(&String::from_utf8_lossy(&bytes));
        }
        if size > room && self.truncated_in.is_none() {
            self.truncated_in = Some(addr.to_owned());
        }
        Ok(())
    }

    fn finish(mut self) -> String {
        if let Some(addr) = self.truncated_in {
            if !self.out.is_empty() && !self.out.ends_with('\n') {
                self.out.push('\n');
            }
            self.out.push_str(&format!(
                "… notice truncated: {} of {} bytes shown; full text: heph inspect outputs {addr}\n",
                self.read, self.total
            ));
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::{CappedText, input_in_group};

    fn render(cap: usize, files: &[(&str, &[u8])]) -> String {
        let mut text = CappedText::new(cap);
        for (addr, body) in files {
            text.file(
                Box::new(std::io::Cursor::new(body.to_vec())),
                body.len() as u64,
                addr,
            )
            .expect("file");
        }
        text.finish()
    }

    /// The notice is the text a human reads before approving a target, so it
    /// concatenates every file in order, and never fails on bytes that are not
    /// UTF-8 — a notice that errored out would block the approval rather than
    /// merely look odd.
    #[test]
    fn notice_text_concatenates_every_file_and_survives_binary_bytes() {
        let out = render(
            1 << 20,
            &[
                ("//p:a", b"first\n"),
                ("//p:a", b"second\n"),
                ("//p:b", &[0xff, 0xfe]),
            ],
        );
        assert!(out.starts_with("first\nsecond\n"), "got {out:?}");
        assert!(
            out.contains('\u{fffd}'),
            "non-UTF-8 bytes must render lossily rather than fail: {out:?}"
        );
        assert!(!out.contains("truncated"), "under the cap: {out:?}");
    }

    /// Past the cap no file is read further, and the text says so: how much
    /// there was, and which input to inspect for the rest. A notice exactly at
    /// the cap is whole.
    #[test]
    fn notice_text_stops_at_the_cap_and_says_where_the_rest_is() {
        assert_eq!(
            render(
                10,
                &[
                    ("//p:a", b"0123"),
                    ("//p:b", b"456789abcd"),
                    ("//p:c", b"xyz")
                ]
            ),
            "0123456789\n… notice truncated: 10 of 17 bytes shown; full text: heph inspect outputs //p:b\n"
        );
        assert_eq!(render(4, &[("//p:a", b"0123")]), "0123");
        assert_eq!(
            render(0, &[("//p:a", b"0123")]),
            "… notice truncated: 0 of 4 bytes shown; full text: heph inspect outputs //p:a\n"
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
