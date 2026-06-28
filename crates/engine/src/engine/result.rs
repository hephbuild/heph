use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::driver::{ApplyTransitiveRequest, ParseRequest, outputartifact};
use crate::engine::error::{
    CancelledError, CycleError, MultiError, ProcessFailed, TargetFailure, TargetNotFoundError,
    UpstreamFailed,
};
use crate::engine::provider::{
    GetError, GetRequest, GetResponse, ListRequest, ProbeRequest, ProviderExecutor, State,
    TargetSpec,
};
use crate::engine::request_state::RequestState;
use crate::engine::spec::EngineTargetSpec;
use async_recursion::async_recursion;
use enclose::enclose;
use hcore::hmemoizer::{downcast_chain_ref, unwrap_arc_err};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::MatchResult;
use hmodel::htpkg::PkgBuf;

use crate::engine::driver::sandbox::Sandbox;
use crate::engine::grow_stack::{GrowStack, grow_stack};
use crate::engine::link::LinkedTargetDef;
use crate::engine::local_cache::{CacheArtifact, Manifest, ManifestArtifactType};
use crate::engine::result_lock::ResultReadGuard;
use anyhow::Context;
use futures::TryStreamExt;
use hcore::defer;
use hcore::hartifactcontent::{Content, ReadSeek, WalkEntry, WalkEntryKind};
use hmodel::htmatcher::Matcher;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use tokio::task::JoinSet;

/// How long to block on the per-addr result lock before surfacing a "waiting on
/// lock" notice (with the holder's pid) to the progress stream. The notice is
/// purely informational; the wait itself continues until acquired or cancelled.
const RESULT_LOCK_NOTICE: std::time::Duration = std::time::Duration::from_secs(5);

/// The boxed future produced by `#[async_recursion]` for `result_addr_impl`,
/// wrapped per-poll by [`GrowStack`] in [`Engine::result_addr`].
type BoxedResultFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Arc<EResult>>> + Send + 'a>>;

/// rs carries the parent addr (set by result_addr via with_parent) so the executor
/// does not need to store it separately.
struct EngineProviderExecutor {
    engine: Weak<Engine>,
    rs: Arc<RequestState>,
}

impl ProviderExecutor for EngineProviderExecutor {
    fn result<'a>(
        &'a self,
        addr: &'a Addr,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
        Box::pin(async move {
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
            engine
                .result_addr(
                    self.rs.clone(),
                    addr,
                    OutputMatcher::All,
                    &ResultOptions::default(),
                )
                .await
        })
    }

    fn note_dep<'a>(
        &'a self,
        addr: &'a Addr,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
        // Edge-only: register parent → addr (the synchronous cycle check) without
        // executing. parent is already set on `rs` by the enclosing result_addr.
        Box::pin(async move { self.rs.track_dep(addr).map_err(anyhow::Error::new) })
    }

    fn query<'a>(
        &'a self,
        m: &'a Matcher,
        extra_skip: &'a [String],
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
        Box::pin(async move {
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
            let rs = self.rs.clone();

            // Collect packages eagerly (non-Send iterator dropped before first await)
            let pkg_iter = engine.packages(m, &rs).await?;
            let pkgs: Vec<String> = pkg_iter.collect::<anyhow::Result<_>>()?;

            let mut result = Vec::new();

            for pkg_str in pkgs {
                let pkg = PkgBuf::from(pkg_str.as_str());

                let states = Arc::clone(&engine).probe_segments(&rs, &pkg).await?;

                for provider in &engine.providers {
                    if rs.skip_providers.contains(&provider.name)
                        || extra_skip.iter().any(|n| n == &provider.name)
                    {
                        continue;
                    }
                    // Collect list results eagerly (non-Send iterator dropped before next await)
                    let list_iter = provider
                        .provider
                        .list(
                            ListRequest {
                                request_id: rs.request_id().to_string(),
                                package: pkg.clone(),
                                states: states
                                    .iter()
                                    .filter(|s| s.provider == provider.name)
                                    .cloned()
                                    .collect(),
                            },
                            rs.ctoken(),
                        )
                        .await?;
                    let raw: Vec<_> = list_iter.collect::<anyhow::Result<_>>()?;

                    for item in raw {
                        let addr = item.addr;
                        if addr.package != pkg {
                            continue;
                        }

                        match m.matches_addr(&addr) {
                            MatchResult::MatchYes => result.push(addr),
                            MatchResult::MatchNo => {}
                            MatchResult::MatchShrug => {
                                // Resolve the candidate's spec/def only to evaluate the
                                // matcher — a speculative inspection, not a dependency. Use a
                                // speculative rs so a rejected candidate leaves no edge in the
                                // shared dep DAG (an edge would close a false cycle later).
                                let spec_rs = rs.speculative();
                                let spec = match Arc::clone(&engine)
                                    .get_spec(spec_rs.clone(), &addr)
                                    .await
                                {
                                    Ok(spec) => Ok(spec),
                                    Err(e)
                                        if downcast_chain_ref::<TargetNotFoundError>(&e)
                                            .is_some() =>
                                    {
                                        continue;
                                    }
                                    // Cycle means this target depends (transitively) on the
                                    // current query caller. It cannot be a dep of the caller
                                    // — skip it from the query results rather than error.
                                    Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => {
                                        continue;
                                    }
                                    res => res,
                                }?;

                                match crate::engine::matcher_spec::match_spec(m, &spec) {
                                    MatchResult::MatchYes => result.push(addr),
                                    MatchResult::MatchNo => {}
                                    MatchResult::MatchShrug => {
                                        let def_res = Arc::clone(&engine)
                                            .get_def(spec_rs.clone(), &addr)
                                            .await;
                                        let def = match def_res {
                                            Ok(def) => def,
                                            // Same as the get_spec branch: cycle means this
                                            // target transitively depends on the query caller —
                                            // it can't be a dep of the caller. Skip it.
                                            Err(e)
                                                if downcast_chain_ref::<CycleError>(&e)
                                                    .is_some() =>
                                            {
                                                continue;
                                            }
                                            Err(e) => return Err(e),
                                        };
                                        if crate::engine::matcher_target::match_target(
                                            m,
                                            &def.target_def,
                                        ) == MatchResult::MatchYes
                                        {
                                            result.push(addr);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            Ok(result)
        })
    }
}

pub struct ExtendedTargetDef {
    pub target_def: Arc<TargetDef>,
    pub applied_transitive: Option<Sandbox>,
    /// Registry name of the driver that produced `target_def`. Folded into
    /// `hashin` so swapping drivers under the same addr invalidates cache —
    /// even if the produced `TargetDef` bytes happen to match.
    pub driver: String,
}

/// Aggregate of a multi-target fanout. `errors` is non-empty only when the
/// request was started with `Engine::new_state_with_fail_fast(false)`; with
/// fail-fast (default), the first error short-circuits `Engine::result` and
/// `errors` stays empty.
#[derive(Default)]
pub struct BatchResult {
    pub ok: Vec<Arc<EResult>>,
    pub errors: Vec<(Addr, anyhow::Error)>,
}

// `EResult` + `ArtifactMeta` now live in the `heph-plugin` contract crate (the
// Driver/Provider trait surface returns them); re-exported here so
// `crate::engine::result::EResult` keeps resolving across the engine + plugins.
pub use hplugin::eresult::{ArtifactMeta, EResult};

/// A cache-backed artifact paired with the read lock guarding its cache entry.
/// Delegates every [`Content`] method to the inner artifact; the guard is held
/// purely for RAII, so the cache entry cannot be overwritten/deleted while any
/// handle to it (here, or cloned into a dependent's sandbox input) is alive. The
/// lock releases when the last `Arc<dyn Content>` for the entry drops.
struct GuardedArtifact {
    inner: Arc<dyn Content>,
    _lock: Arc<ResultReadGuard>,
}

impl Content for GuardedArtifact {
    fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
        self.inner.reader()
    }
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        self.inner.walk()
    }
    fn hashout(&self) -> anyhow::Result<String> {
        self.inner.hashout()
    }
    fn seekable_reader(&self) -> anyhow::Result<Option<Box<dyn ReadSeek + Send>>> {
        self.inner.seekable_reader()
    }
    fn byte_size(&self) -> Option<u64> {
        self.inner.byte_size()
    }
}

/// One produced output of a target as it travels the result pipeline: an opaque
/// [`Content`] handle plus the group/type metadata that [`build_eresult`] and
/// codegen write-back need. The content is either a cache-backed [`CacheArtifact`]
/// (the normal stored/packed path) or a zero-copy passthrough — e.g. an
/// `@heph/fs:file` source file that never entered the local cache, carried as
/// its raw [`OutputArtifact`](outputartifact::OutputArtifact). Passthrough
/// artifacts skip [`Engine::cache_locally`] entirely, so they are never of type
/// `CacheArtifact`.
#[derive(Clone)]
pub struct ResultArtifact {
    pub content: Arc<dyn Content>,
    pub group: String,
    pub r#type: ManifestArtifactType,
}

impl ResultArtifact {
    /// Wrap a cache-backed artifact (the normal stored/packed output).
    fn from_cache(a: CacheArtifact) -> Self {
        Self {
            group: a.group.clone(),
            r#type: a.r#type.clone(),
            content: Arc::new(a),
        }
    }

    /// Wrap a zero-copy passthrough output that skipped the local cache. The
    /// content reads the durable source file directly (no tar, no cache blob),
    /// and — because the file was never snapshotted into the cache — re-hashes
    /// it on consume and fails if it diverges from the hash recorded at
    /// input-hashing time. See [`PassthroughContent`].
    ///
    /// `is_passthrough` only ever flags a `Content::File`; any other variant
    /// reaching here is a producer bug, so it falls back to carrying the raw
    /// artifact rather than panicking.
    fn passthrough(a: outputartifact::OutputArtifact) -> Self {
        let group = a.group.clone();
        let r#type = manifest_artifact_type(&a.r#type);
        let content: Arc<dyn Content> = match a.content {
            outputartifact::Content::File(f) => Arc::new(PassthroughContent {
                source_path: f.source_path,
                out_path: f.out_path,
                x: f.x,
                expected: a.hashout,
            }),
            other => Arc::new(outputartifact::OutputArtifact {
                content: other,
                ..a
            }),
        };
        Self {
            group,
            r#type,
            content,
        }
    }
}

/// [`Content`] for a passthrough source-file artifact (e.g. `@heph/fs:file`):
/// referenced by path and read live on consume, never copied into the cache.
///
/// Because nothing snapshots the bytes, the workspace file could be modified
/// between when it was hashed (the value folded into the target's `hashin` cache
/// key) and when a consumer reads it here. The live bytes would then silently
/// diverge from the cache key, poisoning every downstream entry. To turn that
/// silent corruption into a hard, explicit failure, the bytes are re-hashed as
/// they stream through — no extra I/O, the consumer is reading them anyway — and
/// the digest is checked against the recorded `hashout` at EOF.
///
/// `seekable_reader`/`file_path` stay `None` (Content defaults): the FUSE
/// tar-index path is bypassed and every consumer materializes via `walk()`, so
/// the verifying reader is always on the materialization path.
struct PassthroughContent {
    source_path: String,
    out_path: String,
    x: bool,
    /// Content hash recorded when the target was hashed; the just-read bytes
    /// must still hash to this.
    expected: String,
}

impl PassthroughContent {
    fn verifying_reader(&self) -> anyhow::Result<VerifyingReader> {
        let file = std::fs::File::open(&self.source_path)
            .with_context(|| format!("open passthrough source '{}'", self.source_path))?;
        Ok(VerifyingReader {
            inner: Box::new(file),
            hasher: xxhash_rust::xxh3::Xxh3::new(),
            x: self.x,
            expected: self.expected.clone(),
            source_path: self.source_path.clone(),
            verified: false,
        })
    }
}

impl Content for PassthroughContent {
    fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
        Ok(Box::new(self.verifying_reader()?))
    }

    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        let data: Box<dyn std::io::Read> = Box::new(self.verifying_reader()?);
        Ok(Box::new(std::iter::once(Ok(WalkEntry {
            path: std::path::PathBuf::from(&self.out_path),
            kind: WalkEntryKind::File { data, x: self.x },
        }))))
    }

    fn hashout(&self) -> anyhow::Result<String> {
        Ok(self.expected.clone())
    }
}

/// A `Read` adapter that hashes the bytes it passes through and, at EOF, verifies
/// the digest matches the expected content hash — failing the read (and so the
/// consuming target) otherwise. The algorithm is identical to
/// [`hwalk::file_hashout`]: xxh3 over the content followed by a single exec-bit
/// marker byte. `passthrough_reader_matches_file_hashout` pins the two together
/// so they cannot silently drift.
struct VerifyingReader {
    inner: Box<dyn std::io::Read>,
    hasher: xxhash_rust::xxh3::Xxh3,
    x: bool,
    expected: String,
    source_path: String,
    verified: bool,
}

impl std::io::Read for VerifyingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            // EOF: finalize and verify exactly once. A reader read to completion
            // (the materialization copy always is) triggers this; a partial read
            // that is dropped early simply does not verify.
            if !self.verified {
                self.verified = true;
                self.hasher.update(&[self.x as u8]);
                let got = format!("{:x}", self.hasher.digest());
                if got != self.expected {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "passthrough source file '{}' was modified after it was hashed: \
                             content hash {got} no longer matches the {} recorded at \
                             input-hashing time — a source file changed mid-build",
                            self.source_path, self.expected
                        ),
                    ));
                }
            }
            return Ok(0);
        }
        if let Some(chunk) = buf.get(..n) {
            self.hasher.update(chunk);
        }
        Ok(n)
    }
}

fn manifest_artifact_type(t: &outputartifact::Type) -> ManifestArtifactType {
    match t {
        outputartifact::Type::Output => ManifestArtifactType::Output,
        outputartifact::Type::Log => ManifestArtifactType::Log,
        outputartifact::Type::SupportFile => ManifestArtifactType::SupportFile,
    }
}

/// Whether a produced output is a zero-copy source-file passthrough that must
/// skip the local cache. True only on the uncached (`tmp`) path AND when its
/// producer flagged a `Content::File` as a durable source reference (e.g.
/// `@heph/fs:file`). A cacheable revision must own a durable copy of its bytes
/// (`source_path` may change/vanish across runs), so it is never a passthrough.
fn is_passthrough(use_tmp_cache: bool, content: &outputartifact::Content) -> bool {
    use_tmp_cache && matches!(content, outputartifact::Content::File(f) if f.passthrough)
}

/// Build an [`EResult`] from produced artifacts, filtering by output group and
/// type, and attaching `guard` (the read lock for this target's cache entry) to
/// each kept artifact. `guard` is `None` only for the non-cacheable (force/shell)
/// path, whose artifacts are ephemeral and need no long-lived lock.
fn build_eresult(
    produced: Vec<ResultArtifact>,
    artifacts_meta: Vec<ArtifactMeta>,
    outputs: &[String],
    guard: Option<Arc<ResultReadGuard>>,
) -> EResult {
    let wrap = |content: Arc<dyn Content>| -> Arc<dyn Content> {
        match &guard {
            Some(lock) => Arc::new(GuardedArtifact {
                inner: content,
                _lock: Arc::clone(lock),
            }),
            None => content,
        }
    };

    let mut artifacts: Vec<Arc<dyn Content>> = Vec::new();
    let mut support_artifacts: Vec<Arc<dyn Content>> = Vec::new();
    for a in produced {
        match a.r#type {
            ManifestArtifactType::Output if outputs.contains(&a.group) => {
                artifacts.push(wrap(a.content))
            }
            ManifestArtifactType::SupportFile => support_artifacts.push(wrap(a.content)),
            _ => {}
        }
    }
    EResult {
        artifacts,
        support_artifacts,
        artifacts_meta,
    }
}

/// Stamp the codegen-provenance xattr on a written-back `copy` output. This is
/// REQUIRED, not best-effort: the stamp is what makes a later `glob()`/`file()`
/// exclude the generated file, so a workspace whose filesystem cannot store
/// extended attributes (some tmpfs/NFS/FAT) must FAIL loudly rather than silently
/// emit an unstamped output that would then be double-sourced. (`in_place` outputs
/// are never stamped, so they remain usable on any filesystem.)
fn stamp_codegen_xattr(path: &std::path::Path, value: &str) -> anyhow::Result<()> {
    xattr::set(path, hbuiltins::pluginfs::CODEGEN_XATTR, value.as_bytes()).with_context(|| {
        format!(
            "stamp codegen xattr on {:?}: `codegen = \"copy\"` requires a filesystem with \
             extended-attribute support",
            path
        )
    })
}

pub type InteractiveInner = Box<
    dyn for<'io> FnOnce(
            Option<&'io mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin)>,
            Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
            Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
        ) -> futures::future::BoxFuture<'io, anyhow::Result<()>>
        + Send,
>;

pub type InteractiveWrapper = Arc<
    dyn Fn(InteractiveInner) -> futures::future::BoxFuture<'static, anyhow::Result<()>>
        + Send
        + Sync,
>;

#[derive(Default, Clone)]
pub struct ResultOptions {
    pub force: bool,
    pub shell: bool,
    pub interactive: Option<InteractiveWrapper>,
    /// `--frozen`: verify codegen targets' generated output matches the tree
    /// without writing. A mismatch surfaces a [`FrozenCheckError`].
    pub frozen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutputMatcher {
    None,
    All,
    Exact(Vec<String>),
}

struct ExecuteOptions<'a> {
    hashin: &'a String,
    spec: &'a TargetSpec,
    def: &'a LinkedTargetDef,
    force: bool,
    interactive: Option<InteractiveWrapper>,
    shell: bool,
    frozen: bool,
    /// True only for the directly-requested (top-level) target. Gates codegen
    /// tree write-back so a codegen target pulled in as a *dependency* doesn't
    /// materialize its output into the workspace.
    is_top: bool,
}

/// Output-independent result of the per-addr lock dance, single-flighted by
/// `mem_locked_result` (keyed by `Addr` alone). Every `(outputs, is_top)`
/// memoizer cell of one addr awaits the same cell and shares its single riding
/// read, so two sibling computations can never both contend the non-reentrant
/// per-addr result lock — the self-deadlock this prevents.
pub(crate) struct LockedResolution {
    /// The single riding read shared by all callers, pinning the cache entry for
    /// the request lifetime. `None` only on the non-cacheable force/shell path
    /// (ephemeral artifacts, no long-lived read).
    guard: Option<Arc<ResultReadGuard>>,
    /// `Some(full set)` when THIS cell produced the artifacts (cacheable execute,
    /// or the force/shell branch); [`build_eresult`] filters it to each caller's
    /// outputs. `None` on a pre-existing cache hit — callers then read only THEIR
    /// own outputs from the local cache under the shared `guard`, so a partial /
    /// remote cache pulls just the blobs each caller asked for rather than every
    /// output group (the lazy-pull invariant this preserves).
    executed: Option<Arc<ExecutedArtifacts>>,
    /// The parsed manifest of a pre-existing cache hit, captured once by the
    /// presence-probe and reused by every `(outputs, is_top)` caller in
    /// [`execute_and_cache`] — so each caller filters its own outputs from this
    /// shared manifest instead of re-reading + re-deserializing it from the cache
    /// backend. `Some` exactly when `executed` is `None` (a pre-existing hit);
    /// `None` on the execute / force / shell paths (no pre-existing manifest).
    manifest: Option<Arc<Manifest>>,
}

/// The full freshly-produced artifact set of an executing [`LockedResolution`]
/// cell (every output group), shared across all `(outputs, is_top)` callers and
/// filtered per caller by [`build_eresult`].
struct ExecutedArtifacts {
    cached: Vec<ResultArtifact>,
    meta: Vec<ArtifactMeta>,
}

/// Single classifier chokepoint for any target error.
///
/// Decides whether an error is this target's **own** failure (record it once in
/// the per-request registry, return a fresh `UpstreamFailed{root: addr}`) or
/// merely collateral from a failing dependency (propagate a fresh
/// `UpstreamFailed{root}` without recording). Cancellation is propagated as-is.
///
/// Every collateral hop replaces (never wraps) its incoming error with a fresh
/// `UpstreamFailed`, so chain depth stays O(1) on any graph.
fn classify_failure(
    rs: &RequestState,
    addr: &Addr,
    interactive: bool,
    e: anyhow::Error,
) -> anyhow::Error {
    // Cancellation: propagate unchanged, never record.
    if downcast_chain_ref::<CancelledError>(&e).is_some() {
        return e;
    }

    // Cyclic dependency: a structural error detected at potentially many nodes
    // of the cycle, not a single target's own work failing. Propagate unchanged
    // so the cycle surfaces directly to the caller (and never gets masked behind
    // an `UpstreamFailed` marker or duplicated into the failure registry).
    if downcast_chain_ref::<CycleError>(&e).is_some() {
        return e;
    }

    // Already a collateral marker: reuse the existing root, do not record.
    if let Some(uf) = downcast_chain_ref::<UpstreamFailed>(&e) {
        return UpstreamFailed {
            root: uf.root.clone(),
        }
        .into();
    }

    // Aggregation of child failures. If *every* child is already a recorded
    // collateral marker (or a cancellation), the real root causes live in the
    // registry downstream — this target has no own work to blame, so collapse to
    // a cheap marker without recording. But if any child is a genuine,
    // unrecorded cause (e.g. a `TargetNotFound` raised while resolving an input's
    // def in `link`/`meta`, which never passes through `result_addr`), fall
    // through and record the whole aggregation against this target so the detail
    // (every broken input) isn't lost.
    if let Some(multi) = downcast_chain_ref::<MultiError>(&e) {
        let all_collateral = multi.0.iter().all(|inner| {
            downcast_chain_ref::<UpstreamFailed>(inner).is_some()
                || downcast_chain_ref::<CancelledError>(inner).is_some()
        });
        if all_collateral {
            let root = multi
                .0
                .iter()
                .find_map(|inner| {
                    downcast_chain_ref::<UpstreamFailed>(inner).map(|u| u.root.clone())
                })
                .unwrap_or_else(|| addr.clone());
            return UpstreamFailed { root }.into();
        }
    }

    // This target's own failure (or an aggregation of unrecorded causes): record
    // the rich diagnostic once (first-writer-wins) and propagate a cheap marker.
    // Interactive targets stream their output straight to the user's terminal as
    // they run, so the captured log tail is redundant — drop it from the box.
    let log_tail = if interactive {
        None
    } else {
        extract_log_tail(&e, rs.log_tail_lines())
    };
    rs.record_failure(
        addr.clone(),
        Arc::new(TargetFailure::new(addr.clone(), log_tail, e)),
    );
    UpstreamFailed { root: addr.clone() }.into()
}

/// Read the last `n` lines of a `ProcessFailed`'s log (anywhere in the chain) so
/// the recorded `TargetFailure` can surface them in its diagnostic, tagged with
/// the real starting line number. Best-effort: a log file that can't be read
/// (e.g. already reclaimed) yields no tail rather than masking the failure.
fn extract_log_tail(e: &anyhow::Error, n: usize) -> Option<hplugin::error::LogTail> {
    use std::io::Read as _;
    let pf = downcast_chain_ref::<ProcessFailed>(e)?;
    let mut buf = String::new();
    pf.log.reader().ok()?.read_to_string(&mut buf).ok()?;
    let (text, start_line) = hplugin::error::last_n_lines_with_start(&buf, n);
    Some(hplugin::error::LogTail { text, start_line })
}

/// At the outermost `result_addr` frame (the directly-requested target, with no
/// parent), replace the lightweight `UpstreamFailed` marker with a clone of the
/// rich recorded `TargetFailure` so direct API/library consumers get the real
/// root cause rather than "dependency failed". The CLI renders from the registry
/// instead, so this is purely about the value returned to direct callers. No-op
/// for inner frames and for non-marker errors (cancellation, cycles, …).
fn surface_top(is_top: bool, rs: &RequestState, e: anyhow::Error) -> anyhow::Error {
    if !is_top {
        return e;
    }
    let is_marker = downcast_chain_ref::<UpstreamFailed>(&e).is_some()
        || downcast_chain_ref::<MultiError>(&e).is_some();
    if !is_marker {
        return e;
    }
    // Prefer the rich failure for the marker's named root.
    if let Some(tf) =
        downcast_chain_ref::<UpstreamFailed>(&e).and_then(|uf| rs.get_failure(&uf.root))
    {
        return anyhow::Error::new((*tf).clone());
    }
    // Named root wasn't recorded (e.g. a link-time resolution aggregation whose
    // causes were recorded against the individual deps). Surface the first
    // recorded root cause — there is always at least one for a real failure.
    if let Some(tf) = rs.first_failure() {
        return anyhow::Error::new((*tf).clone());
    }
    e
}

impl Engine {
    /// Resolve a target's result. Thin wrapper over [`Self::result_addr_impl`]
    /// that grows the physical stack on demand.
    ///
    /// Every dependency-graph edge recurses through here, and a cache-warm
    /// descent polls the whole subtree synchronously in one go (~100 KiB of
    /// stack per level). Wrapping each level's poll in [`grow_stack`] keeps that
    /// cascade from overflowing the 2 MiB tokio worker stack on deep graphs.
    /// See `engine::grow_stack` for the full rationale.
    pub fn result_addr<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &'a Addr,
        outputs: OutputMatcher,
        opts: &'a ResultOptions,
    ) -> GrowStack<BoxedResultFuture<'a>> {
        grow_stack(self.result_addr_impl(rs, addr, outputs, opts))
    }

    #[async_recursion]
    async fn result_addr_impl(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<Arc<EResult>> {
        if opts.shell && opts.interactive.is_none() {
            anyhow::bail!("cannot use --shell in non-interactive mode");
        }

        // Stop the moment the request is cancelled (Ctrl-C). Every queued
        // target in a batch enters here; bailing before spec/def resolution
        // and execution means a cancelled build doesn't start new work — it
        // just unwinds. In-flight targets that are already past this point are
        // aborted by their driver's cancellation handling.
        if rs.ctoken().is_cancelled() {
            return Err(CancelledError.into());
        }

        // Announce worker capacity once per request. Covers the single-target
        // entry (`run` of one addr) that bypasses `Engine::result`; the once-guard
        // makes the dep recursion below a no-op.
        rs.announce_max_workers(self.max_workers);

        // Single-target entry (`run` of one addr, which bypasses `Engine::result`):
        // claim the matched stream and emit the set-of-one as already-complete
        // (no `~`). The once-guard keeps this silent for dep recursion and for
        // result_addr calls under a batch `result` (which already claimed) — only
        // a genuine top-level single addr wins the claim here.
        if rs.claim_matched_stream() {
            rs.emit(crate::engine::event::BuildEventKind::Matched {
                addrs: vec![addr.format()],
                complete: true,
            });
        }

        // Directly-requested target (no parent) — the outermost frame. Used below
        // to surface the rich recorded failure to the caller instead of the
        // internal `UpstreamFailed` marker.
        let is_top = rs.parent.is_none();

        // Cycle check: fires for every caller (including those awaiting an in-flight future)
        // before the memoizer blocks, preventing memoizer deadlocks on dependency cycles.
        rs.track_dep(addr).map_err(anyhow::Error::new)?;

        // Set addr as parent so all sub-calls carry the right context for cycle detection.
        // Done outside the memoizer so context setup isn't buried in the deduplication boundary.
        let rs = rs.with_parent(addr.clone());

        // Transparent targets (groups) never execute — inline their deps' results.
        // Handled before the memoizer: nothing to deduplicate for groups, and calling
        // result_addr recursively here is safe because #[async_recursion] boxes the future,
        // breaking the Send inference cycle that would occur inside the memoizer closure.
        //
        // Use _no_track: result_addr just updated `parent → addr` above and set parent=addr;
        // calling tracked get_def would try to record addr→addr (spurious self-cycle).
        let def = match Arc::clone(&self).get_def_no_track(rs.clone(), addr).await {
            Ok(def) => def,
            Err(e) => {
                return Err(surface_top(
                    is_top,
                    &rs,
                    classify_failure(&rs, addr, opts.interactive.is_some(), e),
                ));
            }
        };
        if def.target_def.transparent {
            let mut opts = opts.clone();
            if opts.shell {
                opts.interactive = None;
            }

            let futures: Vec<_> = def
                .target_def
                .inputs
                .iter()
                .map(|input| {
                    let dep_addr = input.r#ref.r#ref.clone();
                    enclose!((self => engine, rs, opts) async move {
                        engine
                            .result_addr(rs, &dep_addr, OutputMatcher::All, &opts)
                            .await
                    })
                })
                .collect();
            let results =
                match crate::engine::fanout::join_all_failable(futures, rs.fail_fast()).await {
                    Ok(results) => results,
                    Err(e) => {
                        return Err(surface_top(
                            is_top,
                            &rs,
                            classify_failure(&rs, addr, opts.interactive.is_some(), e),
                        ));
                    }
                };
            let mut merged = EResult::default();
            for r in results {
                merged.artifacts.extend(r.artifacts.iter().cloned());
                merged
                    .support_artifacts
                    .extend(r.support_artifacts.iter().cloned());
                merged
                    .artifacts_meta
                    .extend(r.artifacts_meta.iter().cloned());
            }
            return Ok(Arc::new(merged));
        }

        // Sort Exact output names so distinct caller-side orderings of the same
        // logical output set share one memoizer entry.
        let mut key_outputs = outputs.clone();
        if let OutputMatcher::Exact(names) = &mut key_outputs {
            names.sort();
        }
        // `is_top` is part of the key: the same target can be reached both
        // top-level (is_top=true) and as a transparent-group member (is_top=false)
        // in one request. Both produce identical artifacts, but only the top-level
        // frame writes the codegen tree back / stores the fixpoint, so each
        // is_top variant needs its own memoizer cell or a race could bake the
        // wrong is_top into the shared computation. The second variant hits the
        // on-disk cache (keyed by hashin, not is_top), so there is no re-execute.
        let key = (addr.clone(), key_outputs, is_top);
        let opts = opts.clone();
        let interactive = opts.interactive.is_some();
        let res = rs
            .data
            .mem_result
            .once(
                key,
                enclose!((self => engine, rs, addr, outputs) move || async move {
                    match engine.inner_result_addr(rs.clone(), &addr, outputs, &opts, is_top).await {
                        Ok(v) => Ok(Arc::new(v)),
                        Err(e) => Err(classify_failure(&rs, &addr, interactive, e)),
                    }
                }),
            )
            .await
            .map_err(unwrap_arc_err);

        match res {
            Ok(v) => Ok(v),
            Err(e) => Err(surface_top(is_top, &rs, e)),
        }
    }

    pub async fn result(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<BatchResult> {
        let mut opts = opts.clone();
        if !matches!(matcher, Matcher::Addr(_)) {
            opts.interactive = None;
        }

        // Announce worker capacity up front so the client can paint a fixed
        // worker-slot indicator before any execute lands.
        rs.announce_max_workers(self.max_workers);

        let fail_fast = rs.fail_fast();
        let mut set: JoinSet<(Addr, anyhow::Result<Arc<EResult>>)> = JoinSet::new();

        // Only the first/top-level `result` streams the matched set. Inner
        // invocations sharing this request's data stay silent — re-emitting
        // would inflate the client's matched count and trip `complete` early.
        let owns_matched = rs.claim_matched_stream();

        // Advertise the matched line up front (provisional, empty set) so the
        // client paints "~0" the instant the query starts, instead of waiting
        // for the first match to stream — the matcher walk can take a while.
        if owns_matched {
            rs.emit(crate::engine::event::BuildEventKind::Matched {
                addrs: Vec::new(),
                complete: false,
            });
        }

        let stream = Arc::clone(&self).query(rs.clone(), matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            // Stop enqueuing new targets once cancelled — don't keep draining
            // the matcher and spawning work that would immediately bail.
            if rs.ctoken().is_cancelled() {
                break;
            }
            // Announce each match as it resolves so the client can render a
            // provisional "done X / ~N" that grows while the matcher streams.
            if owns_matched {
                rs.emit(crate::engine::event::BuildEventKind::Matched {
                    addrs: vec![addr.format()],
                    complete: false,
                });
            }
            hcore::hmemoizer::join_set_spawn(
                &mut set,
                enclose!((self => engine, rs, opts, addr) async move {
                    let r = engine.result_addr(rs, &addr, OutputMatcher::All, &opts).await;
                    (addr, r)
                }),
            );
        }
        // Matcher fully resolved: mark the matched set final (drops the `~`).
        if owns_matched {
            rs.emit(crate::engine::event::BuildEventKind::Matched {
                addrs: Vec::new(),
                complete: true,
            });
        }

        let mut ok: Vec<Arc<EResult>> = vec![];
        let mut errors: Vec<(Addr, anyhow::Error)> = vec![];
        // First genuine (non-cancellation) failure under fail_fast. We never
        // break out of the JoinSet — doing so would drop it and tear down
        // in-flight tasks mid-execution. Instead, the first failure *signals*
        // every other target to stop (cancelling the request token broadcasts
        // SIGINT to running children) and we keep draining until they have all
        // stopped by themselves, then return this error.
        let mut fatal: Option<anyhow::Error> = None;
        while let Some(joined) = set.join_next().await {
            let (addr, res) = match joined {
                Ok(pair) => pair,
                // A task panicked (we never abort tasks). Capture it, signal
                // stop, and keep draining the rest — don't propagate via `?`,
                // which would drop the JoinSet.
                Err(join_err) => {
                    if fatal.is_none() {
                        fatal = Some(anyhow::Error::new(join_err).context("result task panicked"));
                        rs.ctoken().cancel();
                    }
                    continue;
                }
            };
            match res {
                Ok(v) => ok.push(v),
                Err(e) if downcast_chain_ref::<CancelledError>(&e).is_some() => {
                    // Cancellation is stop-fallout, not a genuine failure: the
                    // token is cancelled, so we surface a single `CancelledError`
                    // after draining rather than recording it per-addr.
                }
                Err(e) => {
                    if !fail_fast {
                        errors.push((addr, e));
                    } else if fatal.is_none() {
                        // Fail-fast: tell everything to stop, then wait for it.
                        // Failures that land after we signalled don't override it.
                        fatal = Some(e);
                        rs.ctoken().cancel();
                    }
                }
            }
        }

        if let Some(e) = fatal {
            return Err(e);
        }
        // A cancelled token (Ctrl-C, or a signalled stop) aborts the whole build
        // regardless of `fail_fast`: surface it so the caller reports an abort, not
        // success. Genuine failures collected before the stop remain in the
        // request's failure registry for rich rendering.
        if rs.ctoken().is_cancelled() {
            return Err(CancelledError.into());
        }

        Ok(BatchResult { ok, errors })
    }

    async fn inner_result_addr(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
        opts: &ResultOptions,
        is_top: bool,
    ) -> anyhow::Result<EResult> {
        let addr_str = addr.format();
        crate::engine::event::emit_scope(
            &rs,
            crate::engine::event::BuildEventKind::ResultStart {
                addr: addr_str.clone(),
            },
            move |error| crate::engine::event::BuildEventKind::ResultEnd {
                addr: addr_str,
                error,
            },
            async {
                // Use _no_track: result_addr set parent=addr before entering the memoizer,
                // so tracked variants would record addr→addr.
                let spec = Arc::clone(&self)
                    .get_spec_no_track(rs.clone(), addr)
                    .await?;
                let def = Arc::clone(&self).get_def_no_track(rs.clone(), addr).await?;

                // `link` and `meta` operate on disjoint data once `def` is known: link
                // resolves output names + filter checks across the input list; meta
                // recursively walks inputs to compute hashin. Run them concurrently
                // via `tokio::join!` so the shorter one isn't gated on the longer.
                // Uses `join!` (stack-pinned futures, no per-branch boxing) rather
                // than `try_join_all` — overhead is negligible on the hot path.
                let link_fut = Arc::clone(&self).link(rs.clone(), def.target_def.clone());
                let meta_fut = Arc::clone(&self).meta(rs.clone(), addr);
                let (link_res, meta_res) = tokio::join!(link_fut, meta_fut);
                let def = link_res.with_context(|| "link")?;
                let meta = meta_res.with_context(|| "meta")?;

                let output_names = match outputs {
                    OutputMatcher::None => anyhow::Ok(Vec::<String>::new()),
                    OutputMatcher::All => Ok(def.target.output_names()),
                    OutputMatcher::Exact(names) => {
                        let all_output_names = def.target.output_names();
                        for name in &names {
                            if !all_output_names.contains(name) {
                                anyhow::bail!("output not found: {}", name);
                            }
                        }

                        Ok(names)
                    }
                }?;

                let result = self
                    .execute_and_cache(
                        rs.clone(),
                        &def,
                        output_names,
                        &ExecuteOptions {
                            hashin: &meta.hashin,
                            spec: &spec,
                            def: &def,
                            force: opts.force,
                            interactive: opts.interactive.clone(),
                            shell: opts.shell,
                            frozen: opts.frozen,
                            is_top,
                        },
                    )
                    .await?;

                // Telemetry: artifact count + per-artifact sizes aren't on the
                // event stream, so record them here once the result is in hand.
                // Counts every resolved target across the process; the opt-out
                // only gates whether the snapshot is sent.
                let sizes: Vec<u64> = result
                    .artifacts
                    .iter()
                    .filter_map(|a| a.byte_size())
                    .collect();
                htelemetry::telemetry::record_artifacts(result.artifacts.len() as u64, &sizes);

                Ok(result)
            },
        )
        .await
    }

    /// Acquire a lock guard, surfacing a "waiting on lock" notice (with the
    /// holder's pid) if the wait outlasts [`RESULT_LOCK_NOTICE`]. The notice is
    /// purely informational; the wait continues until acquired or cancelled.
    pub(crate) async fn acquire_with_notice<G>(
        &self,
        rs: &Arc<RequestState>,
        addr: &Addr,
        lock_fut: impl Future<Output = anyhow::Result<G>>,
    ) -> anyhow::Result<G> {
        tokio::pin!(lock_fut);
        match tokio::time::timeout(RESULT_LOCK_NOTICE, &mut lock_fut).await {
            Ok(res) => res.with_context(|| format!("acquiring result lock for {addr}")),
            Err(_elapsed) => {
                let addr_str = addr.format();
                let holder_pid = self.result_lock().holder_pid(addr);
                crate::engine::event::emit_scope(
                    rs,
                    crate::engine::event::BuildEventKind::ResultLockWaitStart {
                        addr: addr_str.clone(),
                        holder_pid,
                    },
                    move |_| crate::engine::event::BuildEventKind::ResultLockWaitEnd {
                        addr: addr_str,
                    },
                    async { (&mut lock_fut).await },
                )
                .await
                .with_context(|| format!("acquiring result lock for {addr}"))
            }
        }
    }

    /// Thin per-`(outputs, is_top)` wrapper over [`resolve_locked`]. The lock
    /// dance runs at most once per addr per request (single-flighted in
    /// `resolve_locked`), producing one shared riding read.
    ///
    /// If that cell executed, it hands back the full freshly-produced set and we
    /// filter it here. On a pre-existing cache hit it hands back only the shared
    /// read, and we fetch *just this caller's* outputs from the local cache under
    /// it — so a partial/remote cache pulls only the blobs each caller asked for,
    /// never the whole output set.
    ///
    /// The codegen write-back + fixpoint registration live here (not in the
    /// shared cell) because they are per-`(outputs, is_top)`: `materialize_codegen`
    /// is is_top-gated, and both run under this caller's shared riding read. This
    /// matches the pre-single-flight placement — `materialize_codegen` on every
    /// path, `maybe_store_fixpoint` only on the cacheable path (never force/shell).
    async fn execute_and_cache(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        outputs: Vec<String>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<EResult> {
        let locked = self.clone().resolve_locked(rs.clone(), def, opts).await?;
        let (cached, meta): (Vec<ResultArtifact>, Vec<ArtifactMeta>) = match &locked.executed {
            // This cell produced the artifacts; filter the full set to `outputs`.
            // Already `ResultArtifact`s (cache-backed or passthrough).
            Some(ex) => (ex.cached.clone(), ex.meta.clone()),
            // Pre-existing hit: read only this caller's outputs under the shared
            // riding read. Silent — the shared cell already emitted the addr's
            // hit/miss event; re-emitting per caller would double-count. Reuse the
            // manifest the probe already parsed (shared across all callers of this
            // single-flight cell) instead of re-reading + re-deserializing it;
            // fall back to a fresh read only if it is somehow absent. A cache hit
            // is always cache-backed (a passthrough never wrote a manifest), so
            // every artifact maps through `from_cache`.
            None => {
                let res = match &locked.manifest {
                    Some(manifest) => {
                        self.artifacts_from_manifest(
                            rs.ctoken(),
                            &def.target.addr,
                            opts.hashin.as_str(),
                            manifest,
                            &outputs,
                        )
                        .await?
                    }
                    None => {
                        self.clone()
                            .artifacts_from_local_cache(
                                rs.ctoken(),
                                def,
                                opts.hashin.as_str(),
                                outputs.clone(),
                            )
                            .await?
                    }
                };
                let (cache_arts, meta) = res.with_context(|| {
                    format!(
                        "result lock confirmed a cache entry for {} but it vanished before read",
                        def.target.addr
                    )
                })?;
                (
                    cache_arts
                        .into_iter()
                        .map(ResultArtifact::from_cache)
                        .collect(),
                    meta,
                )
            }
        };

        // Codegen tree write-back: is_top-gated, idempotent, runs on every path
        // (a cache hit on an in_place fmt must still materialize). Uses this
        // caller's `cached`, so the is_top requester must have asked for the
        // codegen output groups — exactly as before the single-flight split.
        self.materialize_codegen(opts.is_top, opts.def, &cached, opts.frozen)
            .await?;
        // Fixpoint registration only on the cacheable path (force/shell never
        // cache a fixpoint). Idempotent across hit/miss; a no-op unless this is a
        // top-level in_place codegen target whose tree just moved.
        let can_cache = !opts.force && opts.def.target.cache.enabled && !opts.shell;
        if can_cache {
            self.clone().maybe_store_fixpoint(&rs, opts).await?;
        }

        Ok(build_eresult(cached, meta, &outputs, locked.guard.clone()))
    }

    /// Single-flight the per-addr result-lock + cache-fetch/execute, keyed by
    /// `Addr` ALONE (not `outputs`/`is_top`). All `(outputs, is_top)` cells of
    /// one addr await this one cell and share its single riding read, so two
    /// sibling computations of the same addr can never both hold the
    /// non-reentrant per-addr lock (the self-deadlock this prevents).
    ///
    /// Sound because the on-disk cache is keyed by `hashin` (independent of
    /// `outputs`/`is_top`) and execution always produces the full output set:
    /// the shared cell only decides build-vs-hit and hands back one riding read.
    /// Each caller then materializes just its own outputs (see
    /// [`execute_and_cache`](Self::execute_and_cache)), so the pull stays lazy.
    ///
    /// Invariants preserved: cross-process serialization (still exactly one real
    /// flock acquire here; other processes have their own `mem_locked_result`),
    /// cross-request contention (the cell is per-`RequestStateData`), and
    /// read-pinning (the riding read is a real flock read — `try_write` still
    /// fails while it's alive). Only the *duplicate* same-addr acquire is removed.
    async fn resolve_locked(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<Arc<LockedResolution>> {
        let addr = def.target.addr.clone();
        // Owned copies so the memoizer closure is `'static`.
        let def_owned = opts.def.clone();
        let hashin = opts.hashin.clone();
        let spec = opts.spec.clone();
        let force = opts.force;
        let shell = opts.shell;
        let interactive = opts.interactive.clone();

        rs.data
            .mem_locked_result
            .once(
                addr,
                enclose!((self => engine, rs) move || async move {
                    // is_top/frozen are deliberately fixed here: the shared cell
                    // is addr-keyed and output/is_top-agnostic. It never runs
                    // codegen write-back or fixpoint storage (those are per-caller,
                    // in `execute_and_cache`), and neither `execute_and_cache_inner`
                    // nor `execute` reads these fields — so the values are inert.
                    let opts = ExecuteOptions {
                        hashin: &hashin,
                        spec: &spec,
                        def: &def_owned,
                        force,
                        interactive,
                        shell,
                        frozen: false,
                        is_top: false,
                    };
                    engine.resolve_locked_inner(rs, &opts).await
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Body of the per-addr lock dance: the optimistic read → miss → drop →
    /// write → re-check → execute → downgrade → riding-read sequence (and the
    /// non-cacheable force/shell branch). Runs once per addr per request via
    /// [`resolve_locked`].
    ///
    /// Cache presence is decided at the **manifest** level, not per output:
    /// `cache_locally` writes the manifest last and GC deletes whole revisions
    /// under this very lock, so a present manifest guarantees every output blob
    /// for this `hashin` is already on disk. Probing with an empty output set
    /// therefore answers "has this addr been built?" without forcing any output
    /// blob to be fetched — each caller pulls only its own outputs afterwards in
    /// [`execute_and_cache`], keeping partial/remote pulls lazy.
    ///
    /// **Remote-cache contract (TODO(remote-cache)) — keep this split intact:**
    /// - *Presence (here):* must stay manifest-only. When remote lands, this
    ///   becomes a manifest-exists check that consults **local OR remote** and
    ///   downloads NO output blobs — a remote manifest still means "already
    ///   built, do not execute". Note the empty-output probe below is not a true
    ///   manifest-only check: it still `exists`-checks SupportFile blobs, so once
    ///   support files can live remote-only it must be replaced by an explicit
    ///   `manifest_exists(addr, hashin)` (local-or-remote, no blob I/O).
    /// - *Materialization (per caller, in [`execute_and_cache`]):* this is where
    ///   lazy download belongs. The local read there becomes local→remote: use
    ///   the blob if local, else download just the requested output group(s);
    ///   only a genuine local+remote miss returns `None` (→ execute). Each caller
    ///   pulls only the groups it asked for — never the full output set.
    /// - *Hazard:* that per-caller download writes blobs into the local cache
    ///   while holding only the riding **read** lock. It is content-addressed by
    ///   `hashin` (idempotent), but a concurrent GC `try_write` could race a
    ///   half-written blob — the download path will need atomic
    ///   rename-into-place or a per-blob write guard.
    async fn resolve_locked_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<Arc<LockedResolution>> {
        let def = opts.def;
        let can_cache = !opts.force && def.target.cache.enabled && !opts.shell;
        let addr = &def.target.addr;
        let ctoken = rs.ctoken();

        // Non-cacheable (force/shell): execute under an exclusive write lock —
        // serializing per addr across requests/processes — and return ephemeral
        // artifacts with no long-lived read lock.
        if !can_cache {
            // pluginfs targets are pure, ephemeral filesystem reads (cache off):
            // no cross-process state to serialize and GC never touches them, so
            // the per-addr write lock is pure overhead. Skip it.
            // TODO(targetdef): expose this as an explicit flag on TargetDef
            // (e.g. `needs_lock`) instead of hardcoding the fs driver name here.
            let skip_lock = opts.spec.driver == hbuiltins::pluginfs::DRIVER_NAME;
            let _w = if skip_lock {
                None
            } else {
                Some(
                    self.acquire_with_notice(&rs, addr, self.result_lock().write(addr, ctoken))
                        .await?,
                )
            };
            let (cached, meta) = self
                .clone()
                .execute_and_cache_inner(rs.clone(), opts)
                .await?;
            return Ok(Arc::new(LockedResolution {
                guard: None,
                executed: Some(Arc::new(ExecutedArtifacts { cached, meta })),
                manifest: None,
            }));
        }

        // 1. Optimistically take a plain shared read lock and probe the cache at
        //    the manifest level (empty output set — see the doc comment). The read
        //    rides with every caller's artifacts, protecting the entry while in use.
        let read = self
            .acquire_with_notice(&rs, addr, self.result_lock().read(addr, ctoken))
            .await?;
        if let Some(manifest) = self.probe_cache_manifest(&rs, def, opts).await? {
            // A. Hit — share this read; each caller reads its own outputs under
            // it and runs its own codegen write-back / fixpoint in
            // `execute_and_cache` (is_top-gated, under this riding read). The
            // parsed manifest rides along so callers skip a second read+parse.
            return Ok(Arc::new(LockedResolution {
                guard: Some(Arc::new(read)),
                executed: None,
                manifest: Some(manifest),
            }));
        }

        // B. Miss: a plain read cannot upgrade. Drop it and take the exclusive
        //    write lock directly — after a miss we'll almost certainly execute,
        //    so this skips the upgradable→upgrade two-step. It also serializes
        //    the execute phase per addr, replacing the old exclusive result lock.
        drop(read);
        let write = self
            .acquire_with_notice(&rs, addr, self.result_lock().write(addr, ctoken))
            .await?;

        // Re-check (manifest level) under the write lock: covers the drop window
        // above and any writer that produced the artifacts while we waited. The
        // write lock excludes all others, so one re-check suffices. On the rare
        // race-win we share without executing; otherwise we execute + cache the
        // full set, which `build_eresult` filters per caller.
        let (executed, manifest) = match self.probe_cache_manifest(&rs, def, opts).await? {
            Some(manifest) => (None, Some(manifest)),
            None => {
                // Local miss under the write lock: try to pull a complete
                // revision from the remote caches into the local cache before
                // executing. Safe to write local blobs here — the write lock
                // excludes GC and other writers. A remote hit means "already
                // built": skip execution and share the now-local entry.
                //
                // Bracket the pull with one `RemoteCacheRead` span per target
                // (only when a readable cache exists) so a slow download shows as
                // a single `↓` op in the per-target timeline — never one per
                // blob/cache.
                // Honor the target's `remote_enabled`: drivers whose output
                // embeds host-local paths (e.g. the nix driver bakes absolute
                // `/nix/store/...` wrapper paths) set it false so a wrapper built
                // on one machine is never pulled onto another that lacks that
                // store path — which would `exec` a missing path (status 127).
                let downloaded =
                    if def.target.cache.remote_enabled && self.remote_caches.has_readable() {
                        let addr_s = addr.format();
                        crate::engine::event::emit_scope(
                            &rs,
                            crate::engine::event::BuildEventKind::RemoteCacheReadStart {
                                addr: addr_s.clone(),
                            },
                            move |error| crate::engine::event::BuildEventKind::RemoteCacheReadEnd {
                                addr: addr_s,
                                error,
                            },
                            self.download_from_remote(ctoken, addr, opts.hashin.as_str()),
                        )
                        .await?
                    } else {
                        None
                    };
                match downloaded {
                    Some(manifest) => (None, Some(Arc::new(manifest))),
                    None => {
                        let (cached, meta) = self
                            .clone()
                            .execute_and_cache_inner(rs.clone(), opts)
                            .await?;
                        (Some(Arc::new(ExecutedArtifacts { cached, meta })), None)
                    }
                }
            }
        };

        // Downgrade to an upgradable read, then take a plain read while still
        // holding the gateway (gap-free — no writer can delete what we just
        // confirmed/wrote), and release the gateway. The plain read rides with
        // the artifacts.
        let up = write
            .downgrade(ctoken)
            .await
            .with_context(|| format!("downgrading result lock for {addr}"))?;
        let read = self.result_lock().read(addr, ctoken).await?;
        drop(up);
        Ok(Arc::new(LockedResolution {
            guard: Some(Arc::new(read)),
            executed,
            manifest,
        }))
    }

    /// Materialize the codegen output tree for a freshly-resolved target.
    ///
    /// Gated to top-level requested targets (`rs.parent.is_none()`) with at least
    /// one codegen output path. For each codegen output group:
    /// - `frozen`: build a unified diff between the generated bytes and the tree
    ///   file, accumulate per-file diffs, and on any divergence return a
    ///   [`FrozenCheckError`] without writing anything.
    /// - otherwise: unpack the cached artifact into the workspace root (copy
    ///   semantics). `Copy` (net-new) groups additionally stamp every written
    ///   file with the codegen xattr so a later fs glob excludes them; `InPlace`
    ///   groups are not stamped (they overwrite tracked source files).
    async fn materialize_codegen(
        &self,
        is_top: bool,
        def: &LinkedTargetDef,
        cached: &[ResultArtifact],
        frozen: bool,
    ) -> anyhow::Result<()> {
        use crate::engine::driver::targetdef::path::CodegenMode;

        // Gate: only the top-level requested target writes its tree back, and
        // only when it actually declares a codegen output path.
        if !is_top {
            return Ok(());
        }
        let has_codegen = def.target.outputs.iter().any(|o| {
            o.paths
                .iter()
                .any(|p| !matches!(p.codegen_tree, CodegenMode::None))
        });
        if !has_codegen {
            return Ok(());
        }

        let root = &self.cfg.root;
        let mut frozen_diff = String::new();

        // Map each codegen output group to its declared mode (first non-None
        // path wins). One group can back MULTIPLE cached Output artifacts (e.g.
        // several `out` entries sharing a group), so the loop below is driven off
        // the cached artifacts and looks the mode up per artifact — covering every
        // artifact, and exactly once.
        let mut group_mode: std::collections::HashMap<&str, &CodegenMode> =
            std::collections::HashMap::new();
        for output in &def.target.outputs {
            if let Some(mode) = output
                .paths
                .iter()
                .map(|p| &p.codegen_tree)
                .find(|m| !matches!(m, CodegenMode::None))
            {
                group_mode.entry(output.group.as_str()).or_insert(mode);
            }
        }

        for artifact in cached {
            if artifact.r#type != ManifestArtifactType::Output {
                continue;
            }
            let Some(mode) = group_mode.get(artifact.group.as_str()).copied() else {
                continue;
            };
            let group = artifact.group.as_str();

            if frozen {
                // Compare each generated file against the tree; never write.
                let walker = artifact
                    .content
                    .walk()
                    .with_context(|| format!("walk codegen output for frozen check: {group}"))?;
                for entry in walker {
                    let entry = entry
                        .with_context(|| format!("read codegen entry for frozen check: {group}"))?;
                    let (new_bytes, x) = match entry.kind {
                        WalkEntryKind::File { mut data, x } => {
                            let mut buf = Vec::new();
                            std::io::Read::read_to_end(&mut data, &mut buf)
                                .with_context(|| format!("read generated file {:?}", entry.path))?;
                            (buf, x)
                        }
                        WalkEntryKind::Symlink { .. } => continue,
                    };
                    let tree_path = root.join(&entry.path);
                    let old_bytes = match std::fs::read(&tree_path) {
                        Ok(b) => b,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                        Err(e) => {
                            return Err(e).with_context(|| {
                                format!("read tree file {:?} for frozen check", tree_path)
                            });
                        }
                    };
                    let content_same = old_bytes == new_bytes;
                    // The exec bit is part of the (content + exec-bit) fs hash, so
                    // a mode-only divergence is real drift the non-frozen run
                    // would write back — flag it here too, symmetric with the
                    // write-back reconcile.
                    #[cfg(unix)]
                    let exec_same = {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::metadata(&tree_path)
                            .map(|m| (m.permissions().mode() & 0o111 != 0) == x)
                            .unwrap_or(!x)
                    };
                    #[cfg(not(unix))]
                    let exec_same = {
                        // No exec-bit concept on this platform.
                        let _ = x;
                        true
                    };
                    if content_same && exec_same {
                        continue;
                    }
                    let path_label = entry.path.display().to_string();
                    if !content_same {
                        let old = String::from_utf8_lossy(&old_bytes);
                        let new = String::from_utf8_lossy(&new_bytes);
                        let diff = similar::TextDiff::from_lines(old.as_ref(), new.as_ref());
                        // Git-style headers so the per-file path rides on the
                        // `---`/`+++` lines (no redundant label above them).
                        let rendered = diff
                            .unified_diff()
                            .header(&format!("a/{path_label}"), &format!("b/{path_label}"))
                            .to_string();
                        frozen_diff.push_str(&rendered);
                        if !frozen_diff.ends_with('\n') {
                            frozen_diff.push('\n');
                        }
                    }
                    #[cfg(unix)]
                    if !exec_same {
                        let want = if x { "executable" } else { "non-executable" };
                        frozen_diff
                            .push_str(&format!("mode change: {path_label} should be {want}\n"));
                    }
                }
            } else {
                // Materialize the generated tree into the workspace root, but
                // write a file's bytes ONLY when they differ from what's on disk
                // (and reconcile its exec bit separately, below). `@heph/fs`
                // hashes inputs by (content, exec-bit), so re-reading an
                // unchanged file yields the same hash and an idempotent in_place
                // target hits the fixpoint cache instead of re-executing.
                // Skipping identical writes also avoids needless source-control
                // churn and pointless mtime bumps.
                let stamp = def.target.addr.format();
                let walker = artifact
                    .content
                    .walk()
                    .with_context(|| format!("walk codegen output for write-back: {group}"))?;
                for entry in walker {
                    let entry = entry
                        .with_context(|| format!("read codegen entry for write-back: {group}"))?;
                    let dest = root.join(&entry.path);
                    match entry.kind {
                        WalkEntryKind::File { mut data, x } => {
                            let mut new_bytes = Vec::new();
                            std::io::Read::read_to_end(&mut data, &mut new_bytes)
                                .with_context(|| format!("read generated file {:?}", entry.path))?;
                            let unchanged =
                                matches!(std::fs::read(&dest), Ok(old) if old == new_bytes);
                            if !unchanged {
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent).with_context(|| {
                                        format!("create parent dir for {:?}", dest)
                                    })?;
                                }
                                std::fs::write(&dest, &new_bytes)
                                    .with_context(|| format!("write codegen file {:?}", dest))?;
                            }
                            // The exec bit is part of the `@heph/fs` (content,
                            // exec-bit) input hash, so reconcile it to the
                            // generated artifact's `x` even when the bytes are
                            // unchanged — otherwise a target that only flips +x
                            // would never land on disk and the recomputed
                            // fixpoint key would disagree with what ran. Touch
                            // only the exec bits, and only when the boolean
                            // actually differs, so other mode bits stay put and
                            // an already-correct file sees no spurious churn.
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                if let Ok(meta) = std::fs::metadata(&dest) {
                                    let cur = meta.permissions().mode();
                                    if (cur & 0o111 != 0) != x {
                                        let want = if x { cur | 0o111 } else { cur & !0o111 };
                                        std::fs::set_permissions(
                                            &dest,
                                            std::fs::Permissions::from_mode(want),
                                        )
                                        .with_context(
                                            || format!("reconcile exec bit on {:?}", dest),
                                        )?;
                                    }
                                }
                            }
                            // Stamp net-new (Copy) outputs so a later fs glob
                            // excludes them. InPlace outputs overwrite tracked
                            // sources and stay unstamped. (xattr is file metadata,
                            // not content, so it does not perturb the content+x
                            // fs hash.)
                            if matches!(mode, CodegenMode::Copy) {
                                stamp_codegen_xattr(&dest, &stamp)?;
                            }
                        }
                        WalkEntryKind::Symlink { target } => {
                            // Codegen outputs are regular files in practice;
                            // recreate symlinks only when missing or divergent.
                            #[cfg(unix)]
                            {
                                let recreate = match std::fs::read_link(&dest) {
                                    Ok(cur) => cur != target,
                                    Err(_) => true,
                                };
                                if recreate {
                                    if let Some(parent) = dest.parent() {
                                        std::fs::create_dir_all(parent).with_context(|| {
                                            format!("create parent dir for {:?}", dest)
                                        })?;
                                    }
                                    match std::fs::remove_file(&dest) {
                                        Ok(()) => {}
                                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                        Err(e) => {
                                            return Err(e).with_context(|| {
                                                format!("remove {:?} before symlink", dest)
                                            });
                                        }
                                    }
                                    std::os::unix::fs::symlink(&target, &dest).with_context(
                                        || format!("symlink {:?} -> {:?}", dest, target),
                                    )?;
                                }
                                // Stamp Copy symlink outputs too, so a later fs
                                // glob excludes them like regular Copy files.
                                if matches!(mode, CodegenMode::Copy) {
                                    stamp_codegen_xattr(&dest, &stamp)?;
                                }
                            }
                        }
                    }
                }
            }
        }

        if frozen && !frozen_diff.is_empty() {
            return Err(anyhow::Error::new(crate::engine::error::FrozenCheckError {
                addr: def.target.addr.clone(),
                diff: frozen_diff,
            }));
        }

        Ok(())
    }

    // Memoized by addr:hashin — at most one execute+cache cycle runs per target per request,
    // preventing double-execute when the same target is requested with different output matchers.
    async fn execute_and_cache_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<(Vec<ResultArtifact>, Vec<ArtifactMeta>)> {
        let addr = opts.def.target.addr.clone();
        let hashin = opts.hashin.clone();
        let spec = opts.spec.clone();
        let def = opts.def.clone();
        // A secret target is uncached (enforced at parse: secret requires
        // `cache = False`), so `use_tmp_cache` is already true via `!enabled`.
        // `secret` only selects the in-memory secret store in `cache_locally`.
        let secret = opts.def.target.cache.secret;
        let use_tmp_cache = !opts.def.target.cache.enabled || opts.shell;
        let interactive = opts.interactive.clone();
        let shell = opts.shell;
        let key = (addr.clone(), hashin.clone());

        rs.data
            .mem_execute_cache
            .once(
                key,
                enclose!((self => engine, rs) move || async move {
                    hcore::hmemoizer::set_phase("execute_cache:engine_execute");
                    let (artifacts, sandbox_cleanup, sandbox_guards) = engine
                        .clone()
                        .execute(rs.clone(), &addr, &spec, &def, &hashin, interactive, shell)
                        .await
                        .with_context(|| format!("execute {addr}"))?;

                    let artifacts_meta = artifacts
                        .iter()
                        .filter(|a| matches!(
                            a.r#type,
                            outputartifact::Type::Output | outputartifact::Type::SupportFile
                        ))
                        .map(|a| Ok(ArtifactMeta { hashout: a.hashout()? }))
                        .collect::<anyhow::Result<Vec<_>>>()
                        .with_context(|| format!("read artifact metas for {addr}"))?;

                    // SlotGuards drop here too — moved into the defer so
                    // they live across cache_locally (which reads from
                    // the FUSE-side sandbox) and only deregister after
                    // cleanup is enqueued. The cleanup closure is owned
                    // by the bridge that built the sandbox; it knows
                    // whether to rm the plain dir or the FUSE upper.
                    let cleanup_label = format!("{addr}");
                    let bg_pending = rs.bg_pending();
                    defer! {
                        drop(sandbox_guards);
                        if let Some(job) = sandbox_cleanup {
                            crate::engine::sandbox_cleaner::enqueue(cleanup_label, job, bg_pending);
                        }
                    }

                    hcore::hmemoizer::set_phase("execute_cache:cache_locally");

                    // Partition produced outputs. A zero-copy passthrough (an
                    // uncached source file flagged by its producer — e.g.
                    // `@heph/fs:file`) skips the local cache entirely and is
                    // carried as its raw `OutputArtifact`, so the cache-write
                    // hot path does no file read, tar, or copy. Everything else
                    // is packed/stored by `cache_locally`. Order is irrelevant —
                    // `build_eresult` filters by (group, type), not position —
                    // so the two sets are simply concatenated below.
                    let mut passthrough: Vec<ResultArtifact> = Vec::new();
                    let mut to_cache: Vec<outputartifact::OutputArtifact> = Vec::new();
                    for artifact in artifacts {
                        if is_passthrough(use_tmp_cache, &artifact.content) {
                            passthrough.push(ResultArtifact::passthrough(artifact));
                        } else {
                            to_cache.push(artifact);
                        }
                    }

                    // Only emit a LocalCacheWrite span (and run `cache_locally`)
                    // when there is something to store — an all-passthrough
                    // target writes nothing here, so no "cache write" phase.
                    let cached = if to_cache.is_empty() {
                        Ok(Vec::new())
                    } else {
                        let write_addr = addr.format();
                        crate::engine::event::emit_scope(
                            &rs,
                            crate::engine::event::BuildEventKind::LocalCacheWriteStart {
                                addr: write_addr.clone(),
                            },
                            move |error| {
                                crate::engine::event::BuildEventKind::LocalCacheWriteEnd {
                                    addr: write_addr,
                                    error,
                                }
                            },
                            engine.cache_locally(
                                rs.ctoken(),
                                &addr,
                                &hashin,
                                to_cache,
                                use_tmp_cache,
                                secret,
                            ),
                        )
                        .await
                    };

                    let out = cached
                        .map(move |cached| {
                            let mut produced = passthrough;
                            produced
                                .extend(cached.into_iter().map(ResultArtifact::from_cache));
                            (produced, artifacts_meta)
                        })
                        .with_context(|| format!("cache_locally {addr}"));

                    // Remote push: fire-and-forget on a background task (tracked
                    // by `bg_pending`, so the CLI/TUI stays open until it drains).
                    // Cacheable revisions only — tmp/uncacheable are never shared.
                    // `remote_enabled` gates it too: a target whose output embeds
                    // host-local paths (nix wrappers) must never be uploaded, or
                    // another machine pulls a wrapper pointing at a store path it
                    // doesn't have.
                    if out.is_ok() && !use_tmp_cache && def.target.cache.remote_enabled {
                        engine.spawn_remote_upload(&rs, addr.clone(), hashin.clone());
                    }

                    // Post-write GC: trim this target's stale revisions in the
                    // background (same lane as sandbox cleanup), skipping
                    // uncacheable/tmp entries which are ephemeral and would be
                    // dropped anyway. Fire-and-forget; the trim runs only if the
                    // addr's lock is free, so it never blocks the hot path.
                    if out.is_ok() && !use_tmp_cache {
                        let keep = def.target.cache.history;
                        crate::engine::sandbox_cleaner::enqueue(
                            format!("gc {addr}"),
                            Box::new(enclose!((engine, addr, hashin) move || {
                                engine.try_trim_after_write(&addr, keep, &hashin);
                                Ok(())
                            })),
                            rs.bg_pending(),
                        );
                    }

                    hcore::hmemoizer::clear_phase();
                    out
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Register the just-executed in_place target's cache entry under the key a
    /// subsequent run will compute, so re-running an idempotent transform on the
    /// already-transformed tree is a no-op cache hit.
    ///
    /// Called only on the execute path, *after* `materialize_codegen` has written
    /// the transformed files back to the tree. We recompute the target's `hashin`
    /// against that post-write-back state using a *fresh* request (the `@heph/fs`
    /// inputs are cache-off and memoized per request, so a new request re-reads
    /// the just-written files and yields exactly the `hashin` the next run will
    /// see), then duplicate this run's primary cache revision under it.
    ///
    /// This is faithful to how `@heph/fs` actually hashes inputs (content +
    /// exec-bit): the key is derived from the real tree, not from output content
    /// hashes that the next run would never reproduce. Because the hash tracks
    /// content (and the write-back leaves an already-correct file byte-for-byte
    /// identical), re-reading the post-write-back tree reproduces the same key,
    /// so the hit is stable across repeated runs. Best-effort: any failure leaves
    /// the primary entry intact and the next run simply re-executes.
    async fn maybe_store_fixpoint(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<()> {
        use crate::engine::driver::targetdef::path::CodegenMode;

        // Only top-level targets write their tree back, and only in_place targets
        // mutate their own inputs (so only they can reach a fixpoint). Frozen runs
        // never write, hence never cache a fixpoint.
        if opts.frozen || !opts.is_top {
            return Ok(());
        }
        // The fresh request below runs on its own cancellation token (unlinked
        // from this build), so refuse to start once the original build is
        // cancelled — the fixpoint is a pure optimization and must never delay a
        // Ctrl-C teardown.
        if rs.ctoken().is_cancelled() {
            return Ok(());
        }
        let is_in_place = opts.def.target.outputs.iter().any(|o| {
            o.paths
                .iter()
                .any(|p| matches!(p.codegen_tree, CodegenMode::InPlace))
        });
        if !is_in_place {
            return Ok(());
        }

        let addr = opts.def.target.addr.clone();
        // Fresh request → fs inputs re-stat the post-write-back tree, yielding the
        // exact hashin the NEXT run will compute.
        let fresh = self.new_state();
        let fixpoint = match Arc::clone(&self).meta(fresh, &addr).await {
            Ok(m) => m.hashin,
            Err(e) => {
                // Optimization only: the primary entry is already cached.
                tracing::debug!(error = %format!("{e:#}"), %addr, "fixpoint: meta recompute failed");
                return Ok(());
            }
        };
        if fixpoint == *opts.hashin {
            // Tree already at the fixpoint (idempotent run changed nothing).
            return Ok(());
        }
        // Blob copy is synchronous IO — run it off the async poll like every
        // other cache write (see `cache_artifact_locally`).
        let primary = opts.hashin.clone();
        let dup = hproc::process_supervisor::block_or_inline(enclose!(
            (self => engine, addr, fixpoint, primary) move || {
                engine.duplicate_cache_revision(&addr, &primary, &fixpoint)
            }
        ));
        if let Err(e) = dup {
            // Best-effort: the primary cache entry is already written, so a
            // failure here just means the next run re-executes instead of
            // hitting the fixpoint. Never fail a successful build over it.
            tracing::debug!(error = %format!("{e:#}"), %addr, "fixpoint: duplicate cache revision failed");
        }
        Ok(())
    }

    /// Presence-probe the local cache at the manifest level (empty output set —
    /// see the [`resolve_locked_inner`](Self::resolve_locked_inner) doc), emitting
    /// the hit/miss event. On a hit, returns the parsed manifest so the per-caller
    /// output read in [`execute_and_cache`](Self::execute_and_cache) reuses it
    /// instead of re-reading + re-deserializing the manifest from the cache backend.
    ///
    /// Confirming SupportFile blobs exist (via `artifacts_from_manifest` with no
    /// requested outputs) preserves the exact hit/miss semantics of the former
    /// empty-output `artifacts_from_local_cache` probe — a present manifest whose
    /// support blob has been GC'd is still a miss.
    async fn probe_cache_manifest(
        &self,
        rs: &Arc<RequestState>,
        def: &LinkedTargetDef,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<Option<Arc<Manifest>>> {
        let addr = &def.target.addr;
        let hashin = opts.hashin.as_str();
        // TODO(remote-cache): when remote lands, this becomes a manifest-exists
        // check consulting local OR remote and downloading NO output blobs.
        let hit = match self
            .read_manifest_blocking(rs.ctoken(), addr, hashin)
            .await?
        {
            Some(manifest)
                if self
                    .artifacts_from_manifest(rs.ctoken(), addr, hashin, &manifest, &[])
                    .await?
                    .is_some() =>
            {
                Some(Arc::new(manifest))
            }
            _ => None,
        };
        rs.emit(if hit.is_some() {
            crate::engine::event::BuildEventKind::LocalCacheHit {
                addr: addr.format(),
            }
        } else {
            crate::engine::event::BuildEventKind::LocalCacheMiss {
                addr: addr.format(),
            }
        });
        Ok(hit)
    }

    /// Public, tracked. Records `parent → addr` in `dep_dag` and updates `parent`
    /// before delegating to the memoizer. External callers (provider executor,
    /// query stream, `collect_transitive_deps`) use this.
    ///
    /// Internal callers that have already done their own cycle tracking + parent
    /// update (e.g. `result_addr`, `inner_result_addr`, `get_def_inner` resolving
    /// its own spec) must call `get_def_no_track` instead to avoid a spurious
    /// self-edge.
    #[async_recursion]
    pub async fn get_def(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        rs.track_dep(addr).map_err(anyhow::Error::new)?;
        let rs = rs.with_parent(addr.clone());
        self.get_def_no_track(rs, addr).await
    }

    pub async fn get_def_no_track(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        rs.data
            .mem_def
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.get_def_inner(rs, &addr, true).await.with_context(|| format!("get_def: {}", addr))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    pub async fn get_direct_def(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        rs.track_dep(addr).map_err(anyhow::Error::new)?;
        let rs = rs.with_parent(addr.clone());
        self.get_def_inner(rs, addr, false)
            .await
            .with_context(|| format!("get_def: {}", addr))
    }

    async fn get_def_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        apply_transitive: bool,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        // Use _no_track: get_def (or get_direct_def) already updated parent=addr
        // before invoking us. Tracked get_spec here would record addr→addr.
        let spec = Arc::clone(&self)
            .get_spec_no_track(rs.clone(), addr)
            .await?;

        let driver = match self.drivers_by_name.get(&spec.driver) {
            Some(driver) => driver,
            None => anyhow::bail!("driver not found: {}", spec.driver),
        };

        let res = driver
            .driver
            .parse(
                ParseRequest {
                    request_id: rs.request_id().to_string(),
                    target_spec: Arc::clone(&spec.spec),
                },
                rs.ctoken(),
            )
            .await
            .with_context(|| format!("{} parse", driver.name))?;
        let mut def = rewrite_query_inputs(res.target_def, addr, &spec.provider);
        // Expand the engine-baked `//@heph/introspect:outputs` magic input into
        // one `@heph/fs` input per declared output path of this target. Done
        // here (the single seam where `def.inputs` is finalized) so transitive
        // collection below and every downstream consumer — hashing and
        // execution alike — see the synthesized fs inputs and never the magic
        // addr, which no provider serves.
        crate::engine::expand::expand_introspect_inputs(&mut def);

        let all_transitive = if apply_transitive {
            let sb = Arc::clone(&self)
                .collect_transitive_deps(rs.clone(), &def.inputs)
                .await?;

            if sb.empty() { None } else { Some(sb) }
        } else {
            None
        };

        let mut def = match &all_transitive {
            Some(sb) if !sb.empty() => {
                let res = driver
                    .driver
                    .apply_transitive(
                        ApplyTransitiveRequest {
                            request_id: rs.request_id().to_string(),
                            target_def: def,
                            sandbox: sb.clone(),
                        },
                        rs.ctoken(),
                    )
                    .await
                    .with_context(|| "apply transitive")?;

                res.target_def
            }
            _ => def,
        };

        // In-place codegen persists TWO cache revisions per logical input state:
        // the primary (keyed over the pre-transform tree) and the fixpoint (keyed
        // over the post-write-back tree). Double the GC history so `cache.history`
        // still retains that many input *states*, not half as many. Applied here,
        // on the final def, so BOTH GC paths that read `def.cache.history` — the
        // post-write trim and the `heph gc` sweep — inherit it. The exec hash does
        // not include `cache`, so this never invalidates existing cache entries.
        if def.cache.history > 0
            && def.outputs.iter().any(|o| {
                o.paths.iter().any(|p| {
                    matches!(
                        p.codegen_tree,
                        crate::engine::driver::targetdef::path::CodegenMode::InPlace
                    )
                })
            })
        {
            def.cache.history = def.cache.history.saturating_mul(2);
        }

        validate_secret_cache(addr, &def.cache)?;

        if def.hash.is_empty() {
            anyhow::bail!("missing hash");
        }

        Ok(Arc::new(ExtendedTargetDef {
            target_def: Arc::new(def),
            applied_transitive: all_transitive,
            driver: spec.driver.clone(),
        }))
    }

    async fn collect_transitive_deps(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        inputs: &[Input],
    ) -> anyhow::Result<Sandbox> {
        // Hash-only inputs (`hash_deps`) don't participate in the runtime
        // sandbox, so their transitive sandbox state must not leak in either.
        let futures = inputs
            .iter()
            .filter(|i| i.runtime)
            .enumerate()
            .map(|(i, input)| {
                let input_ref = input.r#ref.clone();
                enclose!((self => engine, rs) async move {
                    let spec = Arc::clone(&engine)
                        .get_spec(rs.clone(), &input_ref.r#ref)
                        .await
                        .with_context(|| format!("get spec: {}", input_ref))?;

                    // For transparent targets (groups), use the pre-computed applied_transitive
                    // which already recursively aggregates all nested deps' transitives.
                    // For all other targets, use spec.transitive directly.
                    // Important: avoid calling get_def on non-transparent targets here — get_def
                    // calls collect_transitive_deps which would re-enter the mem_def memoizer
                    // and deadlock on cyclic dep graphs.
                    let transitive = if spec.driver == hbuiltins::plugingroup::DRIVER_NAME {
                        let dep_def = Arc::clone(&engine)
                            .get_def(rs.clone(), &input_ref.r#ref)
                            .await
                            .with_context(|| format!("get def for group: {:?}", input_ref))?;
                        dep_def.applied_transitive.clone()
                    } else {
                        Some(spec.transitive.clone())
                    };

                    if let Some(transitive) = transitive {
                        let id = format!("_transitive_{}_{}", spec.addr.hash_str(), i);
                        anyhow::Ok(Some((id, transitive)))
                    } else {
                        anyhow::Ok(None)
                    }
                })
            });

        let results = crate::engine::fanout::join_all_failable(futures, rs.fail_fast()).await?;

        let mut sb = Sandbox::default();
        for (id, transitive) in results.into_iter().flatten() {
            sb.merge_sandbox(transitive, id);
        }

        Ok(sb)
    }

    /// Public, tracked. Records `parent → addr` in `dep_dag` and updates `parent`
    /// before delegating to the memoizer. External callers use this.
    ///
    /// Internal callers that have already done their own cycle tracking + parent
    /// update must call `get_spec_no_track` instead.
    pub async fn get_spec(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<EngineTargetSpec>> {
        rs.track_dep(addr).map_err(anyhow::Error::new)?;
        let rs = rs.with_parent(addr.clone());
        self.get_spec_no_track(rs, addr).await
    }

    pub async fn get_spec_no_track(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<EngineTargetSpec>> {
        rs.data
            .mem_spec
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.get_spec_inner(&rs, &addr).await
                }),
            )
            .await
            .map_err(|arc| {
                // Preserve typed errors so callers can downcast_ref to them even when
                // the Arc is shared across concurrent memoizer waiters.
                // Reconstruct TargetNotFoundError here (rather than via the chain wrapper)
                // because callers in deeper code use `e.downcast_ref::<TargetNotFoundError>()`
                // at the top level.
                if arc.downcast_ref::<TargetNotFoundError>().is_some() {
                    TargetNotFoundError { addr: addr.clone() }.into()
                } else {
                    unwrap_arc_err(arc)
                }
            })
    }

    /// Probe every registered provider for every parent package of `pkg`, accumulating
    /// the returned `State`s. Mirrors the Go `ProbeSegments` flow.
    ///
    /// Outer memoize per `pkg` (so repeat callers within a request share the result),
    /// inner memoize per `(provider_name, probe_pkg)` so a given provider is probed at
    /// most once per package per request.
    pub async fn probe_segments(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        pkg: &PkgBuf,
    ) -> anyhow::Result<Arc<Vec<State>>> {
        // Single chokepoint for every provider-dispatch path (get/probe/list all
        // route through here), so provider functions are wired before any BUILD eval.
        self.ensure_provider_functions_wired();
        rs.data
            .mem_probe
            .once(
                pkg.clone(),
                enclose!((self => engine, rs, pkg) move || async move {
                    let mut acc: Vec<State> = Vec::new();
                    for probe_pkg in pkg.parent_packages() {
                        for provider in engine.providers.iter() {
                            let inner = rs
                                .data
                                .mem_probe_inner
                                .once(
                                    (provider.name.clone(), probe_pkg.clone()),
                                    enclose!((provider, rs, probe_pkg) move || async move {
                                        let res = provider
                                            .provider
                                            .probe(
                                                ProbeRequest {
                                                    request_id: rs.request_id().to_string(),
                                                    package: probe_pkg,
                                                },
                                                rs.ctoken(),
                                            )
                                            .await?;
                                        Ok(Arc::new(res.states))
                                    }),
                                )
                                .await
                                .map_err(unwrap_arc_err)?;
                            acc.extend(inner.iter().cloned());
                        }
                    }
                    Ok(Arc::new(acc))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn get_spec_inner(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<EngineTargetSpec>> {
        let states = Arc::clone(&self).probe_segments(rs, &addr.package).await?;
        // A provider whose `get()` cycles doesn't preclude a later provider
        // resolving the same addr acyclically (e.g. the go provider over-claims a
        // buildfile codegen target in a Go package dir and drags `go list` into a
        // cycle; the buildfile provider resolves it cleanly). Skip the cyclic
        // provider and keep going; surface the cycle only if NO provider succeeds
        // — never deadlock, never silently drop a resolvable target.
        let mut pending_cycle: Option<anyhow::Error> = None;
        for provider in self.providers.iter() {
            let provider_rs = rs.with_skip_provider(&provider.name);
            let executor: Arc<dyn ProviderExecutor> = Arc::new(EngineProviderExecutor {
                engine: Arc::downgrade(&self),
                rs: provider_rs,
            });

            let spec = match provider
                .provider
                .get(
                    GetRequest {
                        request_id: rs.request_id().to_string(),
                        addr: addr.clone(),
                        states: states
                            .iter()
                            .filter(|s| s.provider == provider.name)
                            .cloned()
                            .collect(),
                        executor: Arc::clone(&executor),
                    },
                    rs.ctoken(),
                )
                .await
            {
                Ok(GetResponse { target_spec }) => target_spec,
                Err(GetError::NotFound) => continue,
                // Return e directly (not bail!(e)) to preserve typed-error downcast
                // through the anyhow::Error chain — required for CycleError handling.
                Err(GetError::Other(e)) => {
                    if downcast_chain_ref::<CycleError>(&e).is_some() {
                        pending_cycle = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            };

            return anyhow::Ok(Arc::new(EngineTargetSpec {
                spec: Arc::new(spec),
                provider: provider.name.clone(),
            }));
        }

        // No provider produced a spec. If one cycled, that's the meaningful
        // failure (hard fail, loud); otherwise the addr is genuinely unknown.
        if let Some(e) = pending_cycle {
            return Err(e);
        }
        Err(TargetNotFoundError { addr: addr.clone() }.into())
    }
}

/// Stamp `_origin = dest.hash_str()` (and, when known, `exclude_provider =
/// dest_provider`) onto any input whose ref points at a query target.
///
/// `_origin` makes each requesting target get its own per-dest variant of the
/// query addr so distinct `mem_spec` cells are computed per dest — the
/// engine-level cycle detector then trips per dest instead of poisoning a
/// shared cell.
///
/// `exclude_provider` ensures the query resolution skips the dest's own
/// provider when iterating candidates. Without it, a provider-emitted target
/// carrying a query input would force the engine to re-iterate the same
/// provider's `list(pkg)` during query resolution, dragging unrelated targets'
/// spec computations into the call stack and opening the door to same-task
/// memoizer re-entrance deadlocks (see `pluginquery::PACKAGE`). User-supplied
/// `exclude_provider` values are not overwritten.
///
/// Hash stability: `def.hash` already covers `def.addr` and these stamps are
/// pure functions of `def.addr` + `dest_provider`. Same dest ⇒ same stamp;
/// different dests live in distinct `mem_def` cells already keyed by addr.
/// No re-hash.
/// A secret target's outputs are held in memory only, so the target must be
/// uncached. Validated centrally here — driver-agnostic — so every driver gets
/// the same contract: the author opts out of caching explicitly (`cache =
/// False`); the engine never silently overrides a caching config.
fn validate_secret_cache(
    addr: &Addr,
    cache: &crate::engine::driver::targetdef::CacheConfig,
) -> anyhow::Result<()> {
    if cache.secret && (cache.enabled || cache.remote_enabled) {
        anyhow::bail!("target {addr} is secret and must be uncached: set `cache = False`");
    }
    Ok(())
}

fn rewrite_query_inputs(
    mut def: crate::engine::driver::targetdef::TargetDef,
    dest: &Addr,
    dest_provider: &str,
) -> crate::engine::driver::targetdef::TargetDef {
    let origin = dest.hash_str();
    for input in &mut def.inputs {
        let r = &input.r#ref.r#ref;
        if r.package.as_str() == hplugin_query::pluginquery::PACKAGE {
            let mut args = r.args.clone();
            args.insert("_origin".to_string(), origin.clone());
            if !dest_provider.is_empty() && !args.contains_key("exclude_provider") {
                args.insert("exclude_provider".to_string(), dest_provider.to_string());
            }
            input.r#ref.r#ref = Addr::new(r.package.clone(), r.name.clone(), args);
        }
    }
    def
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::provider::{
        ConfigRequest, ConfigResponse, ListPackageResponse, ListPackagesRequest, ListResponse,
        ProbeRequest, ProbeResponse,
    };
    use crate::engine::result_lock::{LockBackend, ResultLock};
    use futures::future::BoxFuture;
    use hcore::hasync::{Cancellable, StdCancellationToken};
    use hmodel::htmatcher::Matcher;
    use hmodel::htpkg::PkgBuf;
    use std::collections::BTreeMap;
    use std::sync::Arc as SArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    fn file_output(
        source_path: &str,
        out_path: &str,
        passthrough: bool,
        hashout: &str,
    ) -> outputartifact::OutputArtifact {
        outputartifact::OutputArtifact {
            group: "out".to_string(),
            name: "f".to_string(),
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::File(outputartifact::ContentFile {
                source_path: source_path.to_string(),
                out_path: out_path.to_string(),
                x: false,
                passthrough,
            }),
            hashout: hashout.to_string(),
        }
    }

    /// Passthrough is gated two ways: only on the uncached (`tmp`) path, and only
    /// for a producer-flagged `Content::File`. A cacheable revision, an unflagged
    /// file, or any non-file content is never a passthrough — it must be packed.
    #[test]
    fn is_passthrough_gates_on_tmp_and_producer_flag() {
        let flagged = file_output("/ws/go.mod", "go.mod", true, "h").content;
        let unflagged = file_output("/ws/go.mod", "go.mod", false, "h").content;
        let raw = outputartifact::Content::Raw(outputartifact::ContentRaw {
            data: vec![1, 2, 3],
            path: "x".to_string(),
            x: false,
        });

        assert!(is_passthrough(true, &flagged), "tmp + flagged file");
        assert!(!is_passthrough(false, &flagged), "cacheable must pack");
        assert!(
            !is_passthrough(true, &unflagged),
            "unflagged file must pack"
        );
        assert!(!is_passthrough(true, &raw), "non-file must pack");
    }

    /// A passthrough `ResultArtifact` is the raw `OutputArtifact` as `Content`: it
    /// never becomes a `CacheArtifact`, carries no cache blob, and `walk()` yields
    /// the single source file at its `out_path`, read directly from the durable
    /// `source_path`. `seekable_reader`/`file_path` stay `None` so the FUSE
    /// tar-index path is bypassed and consumers materialize via generic unpack.
    #[tokio::test]
    async fn passthrough_result_artifact_reads_source_without_cache() {
        let dir = tempdir().expect("tempdir");
        let source_path = dir.path().join("go.mod");
        std::fs::write(&source_path, b"module example\n").expect("write");

        let hashout = hwalk::file_hashout(&source_path, false).expect("hash");
        let oa = file_output(
            source_path.to_str().expect("utf8"),
            "mgmt/go/go.mod",
            true,
            &hashout,
        );
        let ra = ResultArtifact::passthrough(oa);

        assert_eq!(ra.group, "out");
        assert_eq!(ra.r#type, ManifestArtifactType::Output);
        // No cache backing: a passthrough exposes neither a seekable tar nor a
        // cache file path.
        assert!(ra.content.seekable_reader().expect("seekable").is_none());
        assert!(ra.content.file_path().is_none());

        let mut walk = ra.content.walk().expect("walk");
        let entry = walk.next().expect("one entry").expect("ok");
        assert!(walk.next().is_none(), "single file");
        assert_eq!(entry.path, std::path::PathBuf::from("mgmt/go/go.mod"));
        let WalkEntryKind::File { mut data, .. } = entry.kind else {
            panic!("expected file entry");
        };
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut data, &mut buf).expect("read");
        assert_eq!(buf, b"module example\n");
    }

    /// A passthrough file is referenced by path and read live, never snapshotted
    /// into the cache. If the workspace file is modified between hashing and
    /// consume, its content no longer matches the recorded `hashout` — which is
    /// folded into the cache key — so reading it to EOF must fail explicitly
    /// rather than silently feed divergent bytes into a downstream cache entry.
    #[tokio::test]
    async fn passthrough_read_fails_when_source_modified_after_hashing() {
        let dir = tempdir().expect("tempdir");
        let source_path = dir.path().join("go.mod");
        std::fs::write(&source_path, b"module example\n").expect("write");

        // Hash recorded at input-hashing time.
        let hashout = hwalk::file_hashout(&source_path, false).expect("hash");

        // File mutated after hashing, before the consumer reads it.
        std::fs::write(&source_path, b"module tampered\n").expect("rewrite");

        let oa = file_output(
            source_path.to_str().expect("utf8"),
            "mgmt/go/go.mod",
            true,
            &hashout,
        );
        let ra = ResultArtifact::passthrough(oa);

        let mut walk = ra.content.walk().expect("walk");
        let entry = walk.next().expect("one entry").expect("ok");
        let WalkEntryKind::File { mut data, .. } = entry.kind else {
            panic!("expected file entry");
        };
        let mut buf = Vec::new();
        let err = std::io::Read::read_to_end(&mut data, &mut buf)
            .expect_err("modified source must fail the read");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains("modified after it was hashed"), "msg: {msg}");
        assert!(msg.contains("go.mod"), "msg names the file: {msg}");
    }

    /// The verifying reader must compute byte-for-byte the same digest as
    /// [`hwalk::file_hashout`] — they are two implementations of one hash and
    /// would silently break passthrough verification if they drifted. Reading an
    /// unmodified file through the reader (with the canonical hash as `expected`)
    /// succeeds; with any other `expected` it fails.
    #[tokio::test]
    async fn passthrough_reader_matches_file_hashout() {
        let dir = tempdir().expect("tempdir");
        let source_path = dir.path().join("blob.bin");
        // Larger than the reader's chunking to exercise multiple `update`s.
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source_path, &bytes).expect("write");

        let canonical = hwalk::file_hashout(&source_path, false).expect("hash");

        let pc = |expected: &str| PassthroughContent {
            source_path: source_path.to_str().expect("utf8").to_string(),
            out_path: "blob.bin".to_string(),
            x: false,
            expected: expected.to_string(),
        };

        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut pc(&canonical).reader().expect("reader"), &mut buf)
            .expect("read");
        assert_eq!(buf, bytes, "bytes pass through unchanged");

        // Same content, wrong expected → the reader rejects at EOF.
        let mut sink = Vec::new();
        std::io::Read::read_to_end(
            &mut pc(&format!("{canonical}0")).reader().expect("reader"),
            &mut sink,
        )
        .expect_err("wrong expected hash must fail");
    }

    /// Minimal [`Content`] for guard-lifetime tests; carries no real bytes.
    struct DummyContent;
    impl Content for DummyContent {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            Ok(Box::new(std::io::empty()))
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("dummy".to_string())
        }
    }

    /// The read lock travels with the artifact, not the `EResult`: it stays held
    /// as long as *any* handle to the artifact is alive — including a handle
    /// cloned into a dependent's sandbox input (or a group target's merged
    /// result) after the producing `EResult` has dropped — and releases only when
    /// the last handle drops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guarded_artifact_holds_read_lock_until_all_handles_drop() {
        let dir = tempdir().expect("tempdir");
        let lock = SArc::new(ResultLock::new(LockBackend::Mem, dir.path().to_path_buf()));
        let addr = Addr::new(PkgBuf::from("pkg"), "x".to_string(), BTreeMap::new());

        let read = lock
            .read(&addr, &StdCancellationToken::new())
            .await
            .expect("read");
        let guarded: Arc<dyn Content> = Arc::new(GuardedArtifact {
            inner: Arc::new(DummyContent),
            _lock: Arc::new(read),
        });
        // A dependent clones the artifact handle into its own structures.
        let cloned = Arc::clone(&guarded);

        // A writer for the same addr blocks while any handle is alive.
        let lock2 = SArc::clone(&lock);
        let addr2 = addr.clone();
        let writer = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            lock2.write(&addr2, &tok).await.map(|_| ())
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!writer.is_finished(), "writer blocked while artifact alive");

        // Producer's EResult drops, but the dependent still holds `cloned`.
        drop(guarded);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !writer.is_finished(),
            "still blocked: the cloned handle keeps the read lock alive"
        );

        drop(cloned);
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("did not hang")
            .expect("join")
            .expect("writer acquires once the last artifact handle drops");
    }

    struct CountingProvider {
        name: String,
        list_calls: SArc<AtomicUsize>,
    }

    impl crate::engine::provider::Provider for CountingProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.name.clone(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            Box::pin(async { Err(GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    /// Provider that emits a `State` from every package it's probed for, and
    /// records every `GetRequest.states` it observes. Used to verify
    /// `probe_segments` walks parent packages and feeds the result into `get`.
    struct ProbeRecorder {
        name: String,
        get_states: SArc<std::sync::Mutex<Vec<Vec<State>>>>,
    }

    impl crate::engine::provider::Provider for ProbeRecorder {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.name.clone(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let states = req.states.clone();
            let recorder = SArc::clone(&self.get_states);
            Box::pin(async move {
                recorder.lock().expect("get_states lock").push(states);
                Err(GetError::NotFound)
            })
        }
        fn probe<'a>(
            &'a self,
            req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            let name = self.name.clone();
            let pkg = req.package.clone();
            Box::pin(async move {
                Ok(ProbeResponse {
                    states: vec![State {
                        package: pkg,
                        provider: name,
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    #[tokio::test]
    async fn probe_segments_walks_parent_packages() -> anyhow::Result<()> {
        let root = tempdir()?;
        let get_states = SArc::new(std::sync::Mutex::new(Vec::<Vec<State>>::new()));
        let get_states_clone = SArc::clone(&get_states);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| {
            Box::new(ProbeRecorder {
                name: "rec".to_string(),
                get_states: SArc::clone(&get_states_clone),
            })
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let states = SArc::clone(&engine)
            .probe_segments(&rs, &PkgBuf::from("a/b/c"))
            .await?;

        let pkgs: Vec<String> = states
            .iter()
            .map(|s| s.package.as_str().to_string())
            .collect();
        assert_eq!(pkgs, vec!["a/b/c", "a/b", "a", ""]);
        for s in states.iter() {
            assert_eq!(s.provider, "rec");
        }

        // get_spec should also forward the accumulated states.
        let addr = Addr::new(PkgBuf::from("a/b/c"), "t".to_string(), Default::default());
        let _ = SArc::clone(&engine).get_spec(rs, &addr).await;
        let recorded = get_states.lock().unwrap();
        assert_eq!(recorded.len(), 1, "get called once");
        let recorded_pkgs: Vec<String> = recorded[0]
            .iter()
            .map(|s| s.package.as_str().to_string())
            .collect();
        assert_eq!(recorded_pkgs, vec!["a/b/c", "a/b", "a", ""]);
        Ok(())
    }

    #[test]
    fn provider_functions_lists_exposed_functions() {
        let root = tempdir().unwrap();
        // `fs` is auto-registered by `Engine::new`.
        let engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .unwrap();
        let fns = engine.provider_functions();
        assert!(
            fns.iter().any(|(p, n, sig)| p == "fs"
                && n == "glob"
                && sig == "glob(pattern: string) -> list[string]"),
            "{fns:?}"
        );
    }

    // Repro: `//...` (PackagePrefix("")) must surface targets in the ROOT
    // package's BUILD file, not just nested packages.
    #[tokio::test]
    async fn query_recursive_includes_root_build_file() -> anyhow::Result<()> {
        let root = tempdir()?;
        // Root-package BUILD.
        std::fs::write(
            root.path().join("BUILD"),
            r#"target(name = "root_t", driver = "d")"#,
        )?;
        // Nested-package BUILD.
        std::fs::create_dir_all(root.path().join("sub"))?;
        std::fs::write(
            root.path().join("sub").join("BUILD"),
            r#"target(name = "sub_t", driver = "d")"#,
        )?;

        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(|init| {
            Box::new(hplugin_buildfile::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
            ))
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let addrs: Vec<Addr> = SArc::clone(&engine)
            .query(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
            .try_collect()
            .await?;

        let names: Vec<&str> = addrs.iter().map(|a| a.name.as_str()).collect();
        assert!(
            names.contains(&"sub_t"),
            "nested target must be found: {names:?}"
        );
        assert!(
            names.contains(&"root_t"),
            "root-package target must be found: {names:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn engine_wires_provider_functions_into_buildfile() -> anyhow::Result<()> {
        // End-to-end: the engine must aggregate `fs`'s exposed `glob` function and
        // inject it into the buildfile provider, so a BUILD calling `heph.fs.glob`
        // resolves at spec time.
        let root = tempdir()?;
        std::fs::write(root.path().join("a.txt"), "")?;
        std::fs::write(root.path().join("b.txt"), "")?;
        std::fs::write(root.path().join("c.md"), "")?;
        std::fs::write(
            root.path().join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("*.txt"))"#,
        )?;

        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        // `fs` is auto-registered by `Engine::new`.
        engine.register_provider(|init| {
            Box::new(hplugin_buildfile::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
            ))
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let addr = Addr::new(PkgBuf::from(""), "t".to_string(), Default::default());
        let spec = SArc::clone(&engine).get_spec(rs, &addr).await?;

        let mut srcs = match spec.spec.config.get("srcs") {
            Some(hcore::htvalue::Value::List(l)) => l
                .iter()
                .map(|e| match e {
                    hcore::htvalue::Value::String(s) => s.clone(),
                    other => panic!("expected string, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("expected list, got {other:?}"),
        };
        srcs.sort();
        assert_eq!(srcs, vec!["a.txt".to_string(), "b.txt".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn skip_providers_excludes_provider_from_query() -> anyhow::Result<()> {
        let root = tempdir()?;
        let list_calls = SArc::new(AtomicUsize::new(0));
        let list_calls_clone = SArc::clone(&list_calls);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| {
            Box::new(CountingProvider {
                name: "test_provider".to_string(),
                list_calls: SArc::clone(&list_calls_clone),
            })
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();
        let skipped_rs = rs.with_skip_provider("test_provider");

        let executor = EngineProviderExecutor {
            engine: SArc::downgrade(&engine),
            rs: skipped_rs,
        };

        let _addrs = executor
            .query(&Matcher::Package(PkgBuf::from("any")), &[])
            .await?;

        assert_eq!(
            list_calls.load(Ordering::SeqCst),
            0,
            "skipped provider must not be called during query"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_engine_result_not_found() -> anyhow::Result<()> {
        let root = tempdir()?;
        let cfg = Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        };

        let engine = Arc::new(Engine::new(cfg)?);
        let rs = engine.new_state();
        let addr = Addr::new(
            PkgBuf::from("non"),
            "existent".to_string(),
            Default::default(),
        );

        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::None, &ResultOptions::default())
            .await;
        assert!(result.is_err());
        let err = result.err().unwrap();

        // The full error chain must mention the address and the not-found cause.
        let full_chain = format!("{:#}", err);
        assert!(
            full_chain.contains("non:existent"),
            "expected addr in error chain: {full_chain}"
        );
        assert!(
            full_chain.contains("target not found"),
            "expected 'target not found' in error chain: {full_chain}"
        );

        Ok(())
    }

    use hbuiltins::pluginstatictarget;
    use std::collections::HashMap;

    fn static_target(addr: &str, labels: &[&str], deps: &[&str]) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        if !deps.is_empty() {
            deps_map.insert("".to_string(), deps.iter().map(|s| s.to_string()).collect());
        }
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            out: HashMap::new(),
            codegen: None,
            deps: deps_map,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Exec target with a custom `run` command (e.g. `"exit 1"` to fail, or a
    /// script that emits log lines). Used by the error-handling tests.
    fn run_target(addr: &str, deps: &[&str], run: &str) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        if !deps.is_empty() {
            deps_map.insert("".to_string(), deps.iter().map(|s| s.to_string()).collect());
        }
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some(run.to_string()),
            out: HashMap::new(),
            codegen: None,
            deps: deps_map,
            labels: vec![],
            ..Default::default()
        }
    }

    /// Target with a named codegen-tree output. Used to exercise matchers
    /// (like `TreeOutputTo`) that only resolve at def level — those force the
    /// executor's query to call `get_def(candidate)`, which is the path the
    /// dep_dag cycle detector guards.
    fn codegen_target(
        addr: &str,
        labels: &[&str],
        out_group: &str,
        deps: &[&str],
    ) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        if !deps.is_empty() {
            deps_map.insert("".to_string(), deps.iter().map(|s| s.to_string()).collect());
        }
        let mut out = HashMap::new();
        out.insert(out_group.to_string(), vec![format!("{out_group}/")]);
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            out,
            codegen: Some("copy".to_string()),
            deps: deps_map,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn engine_with(targets: Vec<pluginstatictarget::Target>) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    /// Like [`engine_with_home`] but wired to a read+write remote cache at
    /// `remote_uri` (a `file://` dir). Used to assert the per-target
    /// `remote_enabled` gate on remote upload/download. The returned `TempDir`
    /// backs the engine's home/lock dirs and must be held alive for the test.
    fn engine_with_remote(
        targets: Vec<pluginstatictarget::Target>,
        remote_uri: &str,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            remote_caches: vec![crate::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: remote_uri.to_string(),
                read: true,
                write: true,
                concurrency: 10,
            }],
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// Exec target that keeps local caching on but disables the remote cache
    /// (`cache = {enabled: true, remote: false}`) — the shape the nix driver
    /// emits because its output embeds host-local `/nix/store` paths.
    fn no_remote_target(addr: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            cache: Some(hcore::htvalue::Value::Map(HashMap::from([
                ("enabled".to_string(), hcore::htvalue::Value::Bool(true)),
                ("remote".to_string(), hcore::htvalue::Value::Bool(false)),
            ]))),
            ..Default::default()
        }
    }

    /// Count regular files under `dir`, recursively. An empty count means the
    /// remote cache received nothing.
    fn count_files(dir: &std::path::Path) -> usize {
        let mut n = 0;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                n += count_files(&path);
            } else {
                n += 1;
            }
        }
        n
    }

    /// Drain background uploads tracked by the request's `bg_pending` counter so
    /// the remote-cache assertions observe a settled state.
    async fn drain_bg(rs: &Arc<crate::engine::request_state::RequestState>) {
        let bg = rs.bg_pending();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while bg.load(Ordering::Acquire) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "bg_pending never drained"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A target that disables its remote cache (`remote_enabled = false`) must
    /// never be uploaded to the remote — otherwise another machine pulls an
    /// artifact whose host-local paths it lacks (the nix-wrapper `exit 127`
    /// bug). The control target (remote on) proves the path is otherwise live.
    #[tokio::test]
    async fn remote_upload_honors_per_target_remote_enabled() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());

        // Remote-disabled target: must leave the remote empty.
        let (engine, _home) = engine_with_remote(vec![no_remote_target("//pkg:off")], &remote_uri)?;
        let addr_off = hmodel::htaddr::parse_addr("//pkg:off")?;
        let rs = engine.new_state();
        engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr_off,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(&rs).await;
        assert_eq!(
            count_files(remote.path()),
            0,
            "remote_enabled=false target must not be uploaded to the remote cache"
        );

        // Control: a default (remote-on) target with the same shape DOES upload,
        // proving the assertion above is the gate doing its job, not a dead path.
        let remote_on = tempdir()?;
        let remote_on_uri = format!("file://{}", remote_on.path().display());
        let (engine, _home) =
            engine_with_remote(vec![static_target("//pkg:on", &[], &[])], &remote_on_uri)?;
        let addr_on = hmodel::htaddr::parse_addr("//pkg:on")?;
        let rs = engine.new_state();
        engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr_on,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(&rs).await;
        assert!(
            count_files(remote_on.path()) > 0,
            "remote-enabled target must upload to the remote cache"
        );
        Ok(())
    }

    /// The symmetric download gate: even when the remote *has* a matching
    /// revision, a `remote_enabled = false` target must execute locally rather
    /// than pull it — pulling would land an artifact whose host-local paths this
    /// machine lacks. Exec's def hash excludes the cache config, so the on/off
    /// targets share a `hashin` and the seeded entry is a genuine candidate.
    #[tokio::test]
    async fn remote_download_honors_per_target_remote_enabled() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;

        // Seed the remote: a default target executes and uploads.
        let (seeder, _seeder_home) =
            engine_with_remote(vec![static_target("//pkg:t", &[], &[])], &remote_uri)?;
        let rs = seeder.new_state();
        seeder
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(&rs).await;
        assert!(
            count_files(remote.path()) > 0,
            "seed must populate the remote"
        );

        // Cold engine, same addr but remote disabled: must execute, never pull.
        let (off, _off_home) = engine_with_remote(vec![no_remote_target("//pkg:t")], &remote_uri)?;
        let (res, events) = resolve_collecting_events(&off, &addr).await;
        res.expect("remote-off target must resolve");
        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:t")
            ),
            "remote-off target must execute locally, not pull: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::RemoteCacheReadStart { addr } if addr == "//pkg:t"
            )),
            "remote-off target must not attempt a remote pull: {events:?}"
        );

        // Control: cold engine, default target, same addr — pulls the seeded
        // entry instead of executing, proving the entry was a live candidate.
        let (on, _on_home) =
            engine_with_remote(vec![static_target("//pkg:t", &[], &[])], &remote_uri)?;
        let (res, events) = resolve_collecting_events(&on, &addr).await;
        res.expect("remote-on target must resolve");
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::RemoteCacheReadStart { addr } if addr == "//pkg:t"
            )),
            "remote-on target must attempt a remote pull: {events:?}"
        );
        assert!(
            !events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:t")
            ),
            "remote-on target must use the remote hit, not execute: {events:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_spec_cross_target_cycle_returns_typed_error() -> anyhow::Result<()> {
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
        ])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let addr_b = hmodel::htaddr::parse_addr("//pkg:b")?;
        let rs = engine.new_state();

        // a→b: succeeds, records edge.
        engine
            .clone()
            .get_spec(rs.with_parent(addr_a.clone()), &addr_b)
            .await?;

        // b→a: would close the cycle. Cycle check fires before memoizer.
        let err = engine
            .clone()
            .get_spec(rs.with_parent(addr_b.clone()), &addr_a)
            .await
            .err()
            .expect("expected cycle error");
        assert!(
            err.downcast_ref::<CycleError>().is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_def_cross_target_cycle_returns_typed_error() -> anyhow::Result<()> {
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
        ])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let addr_b = hmodel::htaddr::parse_addr("//pkg:b")?;
        let rs = engine.new_state();

        engine
            .clone()
            .get_def(rs.with_parent(addr_a.clone()), &addr_b)
            .await?;

        let err = engine
            .clone()
            .get_def(rs.with_parent(addr_b.clone()), &addr_a)
            .await
            .err()
            .expect("expected cycle error");
        assert!(
            err.downcast_ref::<CycleError>().is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    /// Provider whose `get` fails with a typed `CycleError` for one addr (and
    /// `NotFound` otherwise) — models a provider that over-claims a name and
    /// induces a cycle deep in resolution (like the go provider over-claiming a
    /// buildfile codegen target). Used to verify the engine *contains* the
    /// cycle: it falls through to the next provider rather than aborting.
    struct CyclingProvider {
        name: String,
        cycles_for: String,
    }
    impl crate::engine::provider::Provider for CyclingProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.name.clone(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let cycles = req.addr.format() == self.cycles_for;
            let addr = req.addr.clone();
            Box::pin(async move {
                if cycles {
                    Err(GetError::Other(
                        CycleError {
                            from: addr.clone(),
                            to: addr,
                        }
                        .into(),
                    ))
                } else {
                    Err(GetError::NotFound)
                }
            })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    fn engine_with_cycling(
        cycles_for: &str,
        statics: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        // Cycling provider FIRST, so `get_spec` hits it before the static one.
        let cycles_for = cycles_for.to_string();
        engine.register_provider(move |_| {
            Box::new(CyclingProvider {
                name: "cyc".to_string(),
                cycles_for: cycles_for.clone(),
            })
        })?;
        if !statics.is_empty() {
            let provider = pluginstatictarget::Provider::new(statics)?;
            engine.register_provider(move |_| Box::new(provider))?;
        }
        Ok(Arc::new(engine))
    }

    // A provider whose `get` cycles must not abort `get_spec`: the engine falls
    // through to the next provider that resolves the addr acyclically. This is
    // the engine-level containment that makes `q <label> .` find a buildfile
    // target even when the go provider (registered first) over-claims it.
    #[tokio::test]
    async fn get_spec_falls_through_a_cyclic_provider_to_the_next() -> anyhow::Result<()> {
        let engine = engine_with_cycling("//pkg:t", vec![static_target("//pkg:t", &["lbl"], &[])])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;
        let spec = engine.clone().get_spec(engine.new_state(), &addr).await?;
        assert_eq!(
            spec.spec.labels,
            vec!["lbl".to_string()],
            "must resolve via the non-cyclic provider"
        );
        Ok(())
    }

    // When the ONLY provider serving an addr cycles, `get_spec` hard-fails with
    // the typed `CycleError` (loud) — never deadlocks, never silently NotFound.
    #[tokio::test]
    async fn get_spec_hard_fails_when_every_provider_cycles() -> anyhow::Result<()> {
        let engine = engine_with_cycling("//pkg:t", vec![])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;
        let err = engine
            .clone()
            .get_spec(engine.new_state(), &addr)
            .await
            .err()
            .expect("expected a cycle error");
        assert!(
            hcore::hmemoizer::downcast_chain_ref::<CycleError>(&err).is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_request_bails_before_executing() -> anyhow::Result<()> {
        // A request whose token is already cancelled must not resolve or
        // execute the target — it returns CancelledError immediately so a
        // ctrl-c'd build stops starting new work.
        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();
        rs.ctoken().cancel();

        let err = engine
            .clone()
            .result_addr(rs, &addr_a, OutputMatcher::All, &ResultOptions::default())
            .await
            .err()
            .expect("cancelled request must error");
        assert!(
            err.downcast_ref::<CancelledError>().is_some(),
            "expected CancelledError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_input_annotated_with_origin_hash() -> anyhow::Result<()> {
        let engine = engine_with(vec![static_target(
            "//pkg:a",
            &[],
            &["//@heph/query:q@expr=label(foo)"],
        )])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();

        let def = engine.clone().get_def(rs, &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("expected query input");
        let origin = input
            .r#ref
            .r#ref
            .args
            .get("_origin")
            .expect("query input must be annotated with _origin");
        assert_eq!(*origin, addr_a.hash_str());
        Ok(())
    }

    #[tokio::test]
    async fn query_input_annotated_with_exclude_provider() -> anyhow::Result<()> {
        // Auto-injection: the dest's producing provider must be stamped onto
        // query inputs so they can't re-iterate that provider's targets.
        let engine = engine_with(vec![static_target(
            "//pkg:a",
            &[],
            &["//@heph/query:q@expr=label(foo)"],
        )])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let def = engine.clone().get_def(engine.new_state(), &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("expected query input");
        let stamped = input
            .r#ref
            .r#ref
            .args
            .get("exclude_provider")
            .expect("query input must be annotated with exclude_provider");
        // engine_with registers pluginstatictarget under that name.
        assert_eq!(stamped, "pluginstatictarget");
        Ok(())
    }

    #[tokio::test]
    async fn query_input_user_exclude_provider_not_clobbered() -> anyhow::Result<()> {
        let engine = engine_with(vec![static_target(
            "//pkg:a",
            &[],
            &["//@heph/query:q@expr=label(foo),exclude_provider=__user__"],
        )])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let def = engine.clone().get_def(engine.new_state(), &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("expected query input");
        let stamped = input.r#ref.r#ref.args.get("exclude_provider").unwrap();
        assert_eq!(
            stamped, "__user__",
            "user-supplied exclude_provider must not be overwritten"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_spec_returns_engine_target_spec_with_provider_name() -> anyhow::Result<()> {
        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let spec = engine.clone().get_spec(engine.new_state(), &addr_a).await?;
        assert_eq!(
            spec.provider, "pluginstatictarget",
            "EngineTargetSpec must carry the producing provider's name"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_with_cyclic_candidate_skips_and_completes() -> anyhow::Result<()> {
        // //pkg:a is a codegen target whose output tree matches the query's
        // tree_output_to. The matcher resolves only at def level, so the
        // executor's query must call get_def(a). a's def transitively re-asks
        // for the same (per-dest annotated) query → cycle → a must be skipped
        // from its own query result. b is a sibling codegen target with no
        // query dep, so it has no cycle and is included.
        //
        // `exclude_provider=__none__` opts out of the auto-injected
        // exclusion of the dest's own provider — we want intra-provider
        // candidate enumeration here.
        // Matcher pkg is `pkg/gen` because the codegen output of a target at
        // `//pkg:*` with DirPath `gen/` lands in package `pkg/gen`.
        let q = "//@heph/query:q@expr=tree_output(pkg/gen),exclude_provider=__none__";
        let engine = engine_with(vec![
            codegen_target("//pkg:a", &[], "gen", &[q]),
            codegen_target("//pkg:b", &[], "gen", &[]),
        ])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();

        // Must not hang. Pre-fix this either deadlocked or surfaced
        // MemoizerCycleError (when HEPH_DEBUG_MEMOIZER_CYCLE=1).
        let def_a = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            engine.clone().get_def(rs.clone(), &addr_a),
        )
        .await
        .expect("get_def hung — cycle detection failed")?;

        // Pull the annotated query input out of a's def, then call get_spec on it
        // and assert a is excluded from the query result.
        let q_addr = def_a
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("annotated query input")
            .r#ref
            .r#ref
            .clone();
        let q_spec = engine.clone().get_spec(engine.new_state(), &q_addr).await?;
        let deps = match q_spec.config.get("deps") {
            Some(hcore::htvalue::Value::List(l)) => l,
            other => panic!("expected deps list, got {other:?}"),
        };
        let dep_strs: Vec<String> = deps
            .iter()
            .map(|v| match v {
                hcore::htvalue::Value::String(s) => s.clone(),
                other => panic!("expected string dep, got {other:?}"),
            })
            .collect();
        assert!(
            !dep_strs.iter().any(|s| s.starts_with("//pkg:a")),
            "cyclic candidate //pkg:a must be excluded from query result, got {dep_strs:?}"
        );
        assert!(
            dep_strs.iter().any(|s| s.starts_with("//pkg:b")),
            "non-cyclic candidate //pkg:b must be present, got {dep_strs:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_with_two_dests_returns_per_target_results() -> anyhow::Result<()> {
        // Two codegen targets both depending on the tree_output_to query. Per-
        // dest _origin annotation gives each its own mem_spec cell — a's query
        // excludes a but includes b, b's excludes b but includes a. Pre-fix
        // (shared cell) the first to compute would cache a result missing
        // itself, and the second target would see wrong data.
        //
        // `exclude_provider=__none__` opts out of the auto-injected exclusion
        // — we want both same-provider candidates to be enumerable.
        let q = "//@heph/query:q@expr=tree_output(pkg/gen),exclude_provider=__none__";
        let engine = engine_with(vec![
            codegen_target("//pkg:a", &[], "gen", &[q]),
            codegen_target("//pkg:b", &[], "gen", &[q]),
        ])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let addr_b = hmodel::htaddr::parse_addr("//pkg:b")?;
        let rs = engine.new_state();

        let def_a = engine.clone().get_def(rs.clone(), &addr_a).await?;
        let def_b = engine.clone().get_def(rs.clone(), &addr_b).await?;

        let q_addr_a = def_a
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("a's query input")
            .r#ref
            .r#ref
            .clone();
        let q_addr_b = def_b
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("b's query input")
            .r#ref
            .r#ref
            .clone();
        assert_ne!(
            q_addr_a, q_addr_b,
            "per-dest annotation must produce distinct query addrs"
        );

        let extract_deps = |spec: &TargetSpec| -> Vec<String> {
            match spec.config.get("deps") {
                Some(hcore::htvalue::Value::List(l)) => l
                    .iter()
                    .map(|v| match v {
                        hcore::htvalue::Value::String(s) => s.clone(),
                        other => panic!("expected string, got {other:?}"),
                    })
                    .collect(),
                other => panic!("expected deps list, got {other:?}"),
            }
        };

        let spec_a = engine
            .clone()
            .get_spec(engine.new_state(), &q_addr_a)
            .await?;
        let spec_b = engine
            .clone()
            .get_spec(engine.new_state(), &q_addr_b)
            .await?;
        let deps_a = extract_deps(&spec_a);
        let deps_b = extract_deps(&spec_b);

        assert!(
            !deps_a.iter().any(|s| s.starts_with("//pkg:a")),
            "a's query must exclude a, got {deps_a:?}"
        );
        assert!(
            deps_a.iter().any(|s| s.starts_with("//pkg:b")),
            "a's query must include b, got {deps_a:?}"
        );
        assert!(
            !deps_b.iter().any(|s| s.starts_with("//pkg:b")),
            "b's query must exclude b, got {deps_b:?}"
        );
        assert!(
            deps_b.iter().any(|s| s.starts_with("//pkg:a")),
            "b's query must include a, got {deps_b:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn result_fail_fast_on_bails_on_first_failure() -> anyhow::Result<()> {
        // Three targets in `pkg` each depending on a missing target. With
        // fail_fast=true (default), Engine::result must surface Err — no
        // BatchResult is returned.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &["//missing:x"]),
            static_target("//pkg:b", &[], &["//missing:y"]),
            static_target("//pkg:c", &[], &["//missing:z"]),
        ])?;
        let rs = engine.new_state();
        let res = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await;
        assert!(res.is_err(), "fail_fast=true must surface Err");
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_batch_returns_cancelled_not_success() -> anyhow::Result<()> {
        // A cancelled fail-fast batch must abort with CancelledError, not
        // silently report success. The matcher loop stops enqueuing, the
        // JoinSet drains, and the post-drain token check surfaces the abort.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
        ])?;
        let rs = engine.new_state();
        rs.ctoken().cancel();
        let err = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await
            .err()
            .expect("cancelled fail-fast build must return Err");
        assert!(
            downcast_chain_ref::<CancelledError>(&err).is_some(),
            "expected CancelledError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn fail_fast_failure_signals_cancellation_to_siblings() -> anyhow::Result<()> {
        // The user contract: a fail-fast failure does not short-circuit the
        // JoinSet. It signals every other in-flight target to stop (cancels
        // the request token → broadcasts SIGINT) and drains, then surfaces the
        // error. We assert the signal was sent (token cancelled) and the build
        // still returned the failure.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &["//missing:x"]),
            static_target("//pkg:b", &[], &[]),
            static_target("//pkg:c", &[], &[]),
        ])?;
        let rs = engine.new_state();
        let token = rs.ctoken().clone();
        let res = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await;
        assert!(res.is_err(), "fail_fast must surface the failure");
        assert!(
            token.is_cancelled(),
            "fail_fast failure must signal stop to in-flight siblings"
        );
        Ok(())
    }

    #[tokio::test]
    async fn result_fail_fast_off_collects_all_target_failures() -> anyhow::Result<()> {
        // Same setup, fail_fast=false. Every target must be attempted and
        // every per-target error must surface in BatchResult.errors keyed by
        // its own addr — no error is dropped, no early bail.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &["//missing:x"]),
            static_target("//pkg:b", &[], &["//missing:y"]),
            static_target("//pkg:c", &[], &["//missing:z"]),
        ])?;
        let rs = engine.new_state_with_fail_fast(false);
        let batch = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        assert!(batch.ok.is_empty(), "no targets should have succeeded");
        assert_eq!(batch.errors.len(), 3, "expected 3 per-target errors");

        let mut addr_names: Vec<String> =
            batch.errors.iter().map(|(a, _)| a.name.clone()).collect();
        addr_names.sort();
        assert_eq!(addr_names, vec!["a", "b", "c"]);
        Ok(())
    }

    #[tokio::test]
    async fn nested_fail_fast_off_records_aggregated_input_failures() -> anyhow::Result<()> {
        // A parent target with multiple bad inputs (each referencing a missing
        // target). Input *def* resolution happens in link/meta via get_def — not
        // result_addr — so the missing targets never get per-dep registry
        // entries. With fail_fast=false the fanout drives every input to
        // completion and aggregates into a MultiError of unrecorded causes; that
        // aggregation is recorded once against the parent (whose input-resolution
        // work failed), preserving every broken input. The direct caller gets the
        // rich diagnostic via boundary surfacing, never the bare marker.
        use crate::engine::error::{TargetFailure, UpstreamFailed};

        let engine = engine_with(vec![static_target(
            "//pkg:parent",
            &[],
            &["//missing:a", "//missing:b"],
        )])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:parent")?;
        let rs = engine.new_state_with_fail_fast(false);
        let res = engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        let err = res.err().expect("parent must fail");
        assert!(
            downcast_chain_ref::<UpstreamFailed>(&err).is_none(),
            "top-level error must be surfaced as the rich cause, not the marker: {err:#}"
        );
        downcast_chain_ref::<TargetFailure>(&err)
            .expect("expected a surfaced TargetFailure at the boundary");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("missing:a") && rendered.contains("missing:b"),
            "the surfaced failure must list every broken input, got: {rendered}"
        );

        let failures = rs.take_failures();
        assert_eq!(
            failures.len(),
            1,
            "the aggregation is recorded once (against the parent), not duplicated per dep"
        );
        assert_eq!(failures[0].addr.format(), "//pkg:parent");
        Ok(())
    }

    #[tokio::test]
    async fn diamond_failure_recorded_once_at_root() -> anyhow::Result<()> {
        // top → leaf1, leaf2 → base; base fails (its own work errors). Both
        // leaves and top are collateral (they failed only because base did) and
        // must NOT be recorded — only the root cause `base`, exactly once.
        // (The lib harness can't spawn subprocesses, so base fails at exec spawn;
        // the dedup contract is what this exercises — see the e2e suite for real
        // process failures.)
        use crate::engine::error::{TargetFailure, UpstreamFailed};

        let engine = engine_with(vec![
            run_target("//d:base", &[], "exit 1"),
            run_target("//d:leaf1", &["//d:base"], "true"),
            run_target("//d:leaf2", &["//d:base"], "true"),
            run_target("//d:top", &["//d:leaf1", "//d:leaf2"], "true"),
        ])?;
        let addr = hmodel::htaddr::parse_addr("//d:top")?;
        let rs = engine.new_state();
        let err = engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .err()
            .expect("top must fail");

        let failures = rs.take_failures();
        assert_eq!(
            failures.len(),
            1,
            "only the root cause is recorded, not the collateral leaves/top"
        );
        assert_eq!(failures[0].addr.format(), "//d:base");
        // base is recorded as its OWN failure (a genuine cause, not a marker).
        assert!(
            downcast_chain_ref::<UpstreamFailed>(&failures[0].source).is_none(),
            "root cause must be a genuine failure, not an UpstreamFailed marker"
        );
        // The boundary surfaces a rich diagnostic to the direct caller.
        assert!(downcast_chain_ref::<TargetFailure>(&err).is_some());
        Ok(())
    }

    #[test]
    fn deep_chain_failure_has_bounded_error_depth() {
        // A linear chain a0→a1→…→aN where only the tail fails. The error
        // propagated to the caller must NOT accumulate one frame per hop — each
        // collateral hop replaces (never wraps) its incoming error with a fresh
        // marker, so the chain stays O(1). Proven by comparing two very different
        // chain lengths: the surfaced error's depth must be identical, and only
        // the tail is recorded.
        //
        // Run on a large-stack thread with its own runtime: the engine's `meta`
        // walk recurses once per hop and overflows the 2MB default test stack
        // well before the depths exercised here.
        use crate::engine::error::TargetFailure;

        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async {
                    async fn run_chain(n: usize) -> (usize, usize, String) {
                        let addrs: Vec<String> =
                            (0..n).map(|i| format!("//chain:a{i}")).collect();
                        let mut targets = Vec::with_capacity(n);
                        for i in 0..n {
                            if i + 1 < n {
                                targets
                                    .push(run_target(&addrs[i], &[addrs[i + 1].as_str()], "true"));
                            } else {
                                targets.push(run_target(&addrs[i], &[], "exit 1"));
                            }
                        }
                        let engine = engine_with(targets).expect("engine");
                        let head = hmodel::htaddr::parse_addr(&addrs[0]).expect("addr");
                        let rs = engine.new_state();
                        let err = engine
                            .clone()
                            .result_addr(
                                rs.clone(),
                                &head,
                                OutputMatcher::All,
                                &ResultOptions::default(),
                            )
                            .await
                            .err()
                            .expect("head must fail");
                        assert!(downcast_chain_ref::<TargetFailure>(&err).is_some());
                        let failures = rs.take_failures();
                        (
                            failures.len(),
                            err.chain().count(),
                            failures.first().map(|f| f.addr.format()).unwrap_or_default(),
                        )
                    }

                    let (rec_short, depth_short, root_short) = run_chain(10).await;
                    let (rec_long, depth_long, root_long) = run_chain(200).await;

                    assert_eq!(rec_short, 1, "one recorded root cause regardless of length");
                    assert_eq!(rec_long, 1, "one recorded root cause regardless of length");
                    assert_eq!(root_short, "//chain:a9");
                    assert_eq!(root_long, "//chain:a199");
                    assert_eq!(
                        depth_short, depth_long,
                        "error chain depth must be O(1) — independent of graph depth ({depth_short} vs {depth_long})"
                    );
                    assert!(
                        depth_long < 10,
                        "surfaced error must be a shallow chain, got {depth_long}"
                    );
                });
            })
            .expect("spawn")
            .join()
            .expect("join");
    }

    #[tokio::test]
    async fn classify_attaches_process_log_tail_to_recorded_failure() -> anyhow::Result<()> {
        // When a target's own failure carries a `ProcessFailed` (anywhere in the
        // chain), the recorded `TargetFailure` must surface its log tail. Driven
        // directly through `classify_failure` so it's deterministic and doesn't
        // depend on spawning a real subprocess (the lib harness can't; the e2e
        // suite covers the live path). `last_n_lines` itself is unit-tested in
        // `engine::error`.
        use crate::engine::error::{ProcessFailed, UpstreamFailed};
        use std::sync::Arc;

        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let rs = engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        // A 12-line log, default tail of 10 → lines 3..=12 with start_line 3.
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("log.txt");
        let full: String = (1..=12).map(|i| format!("line{i}\n")).collect();
        std::fs::write(&log_path, &full)?;

        let e = anyhow::Error::new(ProcessFailed {
            status: "exit status: 1".to_string(),
            log: Arc::new(hcore::hartifactcontent::FileContent::new(&log_path)),
        })
        .context("driver run")
        .context("execute //pkg:a");

        let out = classify_failure(&rs, &addr, false, e);
        // Own failure → cheap marker propagated upward.
        assert!(downcast_chain_ref::<UpstreamFailed>(&out).is_some());
        // …and the rich record carries the last 10 lines read from the log file,
        // tagged with the real starting line number.
        let recorded = rs.get_failure(&addr).expect("failure must be recorded");
        let tail = recorded.log_tail.as_ref().expect("log tail");
        assert_eq!(
            tail.text,
            "line3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12"
        );
        assert_eq!(tail.start_line, 3);
        Ok(())
    }

    #[tokio::test]
    async fn classify_drops_log_tail_for_interactive_targets() -> anyhow::Result<()> {
        // Interactive targets stream their output live to the user's terminal, so
        // the captured log tail must NOT be re-rendered in the failure box.
        use crate::engine::error::ProcessFailed;
        use std::sync::Arc;

        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let rs = engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("log.txt");
        std::fs::write(&log_path, "line9\nline10\n")?;

        let e = anyhow::Error::new(ProcessFailed {
            status: "exit status: 1".to_string(),
            log: Arc::new(hcore::hartifactcontent::FileContent::new(&log_path)),
        })
        .context("execute //pkg:a");

        let _ = classify_failure(&rs, &addr, true, e);
        let recorded = rs.get_failure(&addr).expect("failure must be recorded");
        assert_eq!(recorded.log_tail, None);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_is_not_recorded_as_failure() -> anyhow::Result<()> {
        // A pre-cancelled request bails with CancelledError before doing work;
        // cancellation is not a target failure and must not be recorded.
        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();
        rs.ctoken().cancel();
        let err = engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .err()
            .expect("cancelled request must fail");
        assert!(downcast_chain_ref::<CancelledError>(&err).is_some());
        assert!(
            rs.take_failures().is_empty(),
            "cancellation must not be recorded as a failure"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cycle_detection_returns_typed_cycle_error() -> anyhow::Result<()> {
        let root = tempdir()?;
        let engine = Arc::new(Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?);
        let addr = Addr::new(PkgBuf::from("p"), "t".to_string(), Default::default());
        // Pre-populate dag with addr→addr already there is overkill; just call result_addr
        // twice with the same parent set, but result_addr sets parent via with_parent so the
        // second invocation inside the same parent chain triggers cycle. Simulate by manually
        // setting rs.parent = addr before calling result_addr(addr).
        let rs = engine.new_state().with_parent(addr.clone());
        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::None, &ResultOptions::default())
            .await;
        assert!(result.is_err(), "expected cycle error");
        let err = result.err().unwrap();
        assert!(
            err.downcast_ref::<CycleError>().is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    use crate::engine::event::{BuildEvent, BuildEventKind};

    fn static_target_run(addr: &str, run: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some(run.to_string()),
            out: HashMap::new(),
            codegen: None,
            deps: HashMap::new(),
            labels: vec![],
            ..Default::default()
        }
    }

    /// Engine + the `TempDir` backing its `home`/cache. The caller must hold the
    /// returned `TempDir` alive for the duration of the test so the on-disk cache
    /// survives across resolves (warm-cache assertions read it back).
    fn engine_with_home(
        targets: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// Resolve `addr` with a fresh event-collecting `RequestState`, then drop the
    /// state (closing the sender) and drain every emitted event.
    async fn resolve_collecting_events(
        engine: &Arc<Engine>,
        addr: &Addr,
    ) -> (anyhow::Result<Arc<EResult>>, Vec<BuildEvent>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        let res = engine
            .clone()
            .result_addr(
                rs.clone(),
                addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        drop(rs);
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        (res, events)
    }

    #[tokio::test]
    async fn emits_result_execute_and_cache_miss_for_fresh_target() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("fresh target must resolve");

        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ResultStart { addr } if addr == "//pkg:a")
            ),
            "expected ResultStart, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ExecuteStart { addr, driver, cache }
                    if addr == "//pkg:a" && driver == "exec" && *cache
            )),
            "expected ExecuteStart{{driver:exec, cache:true}}, got {events:?}"
        );
        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::LocalCacheMiss { addr } if addr == "//pkg:a")
            ),
            "expected LocalCacheMiss, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ExecuteEnd { addr, error: None } if addr == "//pkg:a"
            )),
            "expected ExecuteEnd{{error:None}}, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ResultEnd { addr, error: None } if addr == "//pkg:a"
            )),
            "expected ResultEnd{{error:None}}, got {events:?}"
        );

        // Server-stamped: every event carries a non-zero wall-clock timestamp.
        for e in &events {
            assert!(e.at_unix_ms > 0, "event missing at_unix_ms stamp: {e:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn single_target_result_addr_announces_max_workers_once() -> anyhow::Result<()> {
        // Regression: `run` of a single addr calls `result_addr` directly,
        // bypassing `Engine::result`. The MaxWorkers announcement must still fire
        // (so the TUI paints the worker indicator), and exactly once.
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("fresh target must resolve");

        let max_workers: Vec<usize> = events
            .iter()
            .filter_map(|e| match &e.kind {
                BuildEventKind::MaxWorkers { count } => Some(*count),
                _ => None,
            })
            .collect();
        assert_eq!(
            max_workers.len(),
            1,
            "expected exactly one MaxWorkers event, got {events:?}"
        );
        assert!(max_workers[0] >= 1, "worker count must be positive");
        Ok(())
    }

    #[tokio::test]
    async fn warm_cache_emits_local_cache_hit_and_no_execute() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        // First resolve populates the cache (same engine ⇒ same home/cache).
        let (first, _) = resolve_collecting_events(&engine, &addr).await;
        first.expect("first resolve must succeed");

        // Second resolve on the same engine must hit the local cache.
        let (second, events) = resolve_collecting_events(&engine, &addr).await;
        second.expect("second resolve must succeed");

        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::LocalCacheHit { addr } if addr == "//pkg:a")
            ),
            "warm resolve must emit LocalCacheHit, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.kind, BuildEventKind::ExecuteStart { .. })),
            "warm resolve must not re-execute (no ExecuteStart), got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.kind, BuildEventKind::ExecuteEnd { .. })),
            "warm resolve must not re-execute (no ExecuteEnd), got {events:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failing_target_carries_error_in_execute_and_result_end() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "false")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        assert!(res.is_err(), "run:false target must fail");

        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ExecuteEnd { addr, error: Some(_) } if addr == "//pkg:a"
            )),
            "ExecuteEnd must carry the error (drop-guard on ? path), got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ResultEnd { addr, error: Some(_) } if addr == "//pkg:a"
            )),
            "ResultEnd must carry the error, got {events:?}"
        );
        for e in &events {
            assert!(e.at_unix_ms > 0, "event missing at_unix_ms stamp: {e:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn result_emits_matched_with_resolved_set() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![
            static_target_run("//pkg:a", "true"),
            static_target_run("//pkg:b", "true"),
        ])?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        let batch = engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        assert_eq!(batch.ok.len(), 2);
        drop(rs);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // The matched set streams incrementally (one event per match) and is
        // followed by a final `complete` marker.
        let mut matched: Vec<String> = Vec::new();
        let mut saw_complete = false;
        for e in &events {
            if let BuildEventKind::Matched { addrs, complete } = &e.kind {
                matched.extend(addrs.iter().cloned());
                saw_complete |= *complete;
            }
        }
        assert!(
            saw_complete,
            "result must emit a final complete Matched event"
        );
        assert_eq!(matched.len(), 2, "matched set: {matched:?}");
        assert!(matched.contains(&"//pkg:a".to_string()), "{matched:?}");
        assert!(matched.contains(&"//pkg:b".to_string()), "{matched:?}");
        Ok(())
    }

    #[tokio::test]
    async fn result_emits_provisional_zero_matched_up_front() -> anyhow::Result<()> {
        // The matched line is advertised the instant the query starts: the first
        // Matched event carries an empty set with complete=false (provisional
        // "~0"), before any match has streamed.
        let (engine, _home) = engine_with_home(vec![
            static_target_run("//pkg:a", "true"),
            static_target_run("//pkg:b", "true"),
        ])?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        drop(rs);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let first = events
            .iter()
            .find_map(|e| match &e.kind {
                BuildEventKind::Matched { addrs, complete } => Some((addrs.clone(), *complete)),
                _ => None,
            })
            .expect("a Matched event must be emitted");
        assert_eq!(
            first,
            (Vec::new(), false),
            "first Matched event must advertise an empty, provisional set"
        );
        Ok(())
    }

    #[tokio::test]
    async fn single_addr_result_addr_emits_matched_complete() -> anyhow::Result<()> {
        // The single-addr entry (run of one addr) goes straight to
        // `result_addr`, which must announce the set-of-one as complete.
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drop(rs);

        let mut matched: Vec<String> = Vec::new();
        let mut saw_complete = false;
        while let Ok(ev) = rx.try_recv() {
            if let BuildEventKind::Matched { addrs, complete } = &ev.kind {
                matched.extend(addrs.iter().cloned());
                saw_complete |= *complete;
            }
        }
        assert!(saw_complete, "single-addr must emit complete Matched");
        assert_eq!(matched, vec!["//pkg:a".to_string()], "matched: {matched:?}");
        Ok(())
    }

    #[tokio::test]
    async fn inner_result_does_not_re_emit_matched() -> anyhow::Result<()> {
        // Only the first/top-level `result` owns the matched stream. A second
        // `result` sharing the same request data (the "inner" case) must stay
        // silent so it can't inflate the matched count or trip `complete`.
        let (engine, _home) = engine_with_home(vec![
            static_target_run("//pkg:a", "true"),
            static_target_run("//pkg:b", "true"),
        ])?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));

        // First call claims the stream and emits Matched.
        engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;

        // Drain everything the first call emitted.
        while rx.try_recv().is_ok() {}

        // Second call on the same request data must not emit any Matched event.
        engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        drop(rs);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, BuildEventKind::Matched { .. })),
            "inner result must not emit Matched, got {events:?}"
        );
        Ok(())
    }

    #[test]
    fn validate_secret_cache_requires_uncached() {
        let addr = hmodel::htaddr::parse_addr("//pkg:s").expect("addr");

        // Secret + local cache on → rejected, with an actionable message.
        let mut c = CacheConfig::off();
        c.secret = true;
        c.enabled = true;
        let err = validate_secret_cache(&addr, &c).expect_err("must reject cached secret");
        let msg = format!("{err:#}");
        assert!(msg.contains("secret"), "{msg}");
        assert!(msg.contains("cache = False"), "{msg}");

        // Secret + remote cache on → also rejected.
        let mut c = CacheConfig::off();
        c.secret = true;
        c.remote_enabled = true;
        assert!(validate_secret_cache(&addr, &c).is_err());

        // Secret + fully uncached → ok.
        let mut c = CacheConfig::off();
        c.secret = true;
        assert!(validate_secret_cache(&addr, &c).is_ok());

        // Non-secret cached target → unaffected.
        assert!(validate_secret_cache(&addr, &CacheConfig::on(true)).is_ok());
    }

    // ----------------------------------------------------------------------
    // Per-addr result-lock self-deadlock regression (mem_locked_result).
    // ----------------------------------------------------------------------

    use crate::engine::driver::targetdef::{CacheConfig, Output};
    use crate::engine::driver::{
        ApplyTransitiveResponse, ConfigRequest as DriverConfigRequest,
        ConfigResponse as DriverConfigResponse, Driver as RawDriver, ParseResponse, RunRequest,
        RunResponse,
    };
    use async_trait::async_trait;

    /// Raw driver whose `run` produces one cacheable Raw output per `(group,
    /// name)` in `outputs` and counts executions. Lets the per-addr result-lock
    /// paths be exercised without spawning a subprocess or writing real files.
    struct BlockingDriver {
        exec_count: SArc<AtomicUsize>,
        /// `(output group, artifact name)` pairs this target emits.
        outputs: SArc<Vec<(String, String)>>,
    }

    #[async_trait]
    impl RawDriver for BlockingDriver {
        fn config(&self, _req: DriverConfigRequest) -> anyhow::Result<DriverConfigResponse> {
            Ok(DriverConfigResponse {
                name: "blocking".to_string(),
            })
        }
        fn schema(&self) -> crate::engine::driver::DriverSchema {
            crate::engine::driver::DriverSchema::default()
        }
        async fn parse(
            &self,
            req: ParseRequest,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            Ok(ParseResponse {
                target_def: TargetDef {
                    addr: req.target_spec.addr.clone(),
                    labels: vec![],
                    raw_def: SArc::new(()),
                    inputs: vec![],
                    outputs: self
                        .outputs
                        .iter()
                        .map(|(group, _)| Output {
                            group: group.clone(),
                            paths: vec![],
                        })
                        .collect(),
                    support_files: vec![],
                    cache: CacheConfig::on(false),
                    pty: false,
                    hash: vec![1, 2, 3, 4],
                    transparent: false,
                },
            })
        }
        async fn apply_transitive(
            &self,
            req: ApplyTransitiveRequest,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ApplyTransitiveResponse> {
            Ok(ApplyTransitiveResponse {
                target_def: req.target_def,
            })
        }
        async fn run<'a, 'io>(
            &self,
            _req: RunRequest<'a, 'io>,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<RunResponse> {
            self.exec_count.fetch_add(1, Ordering::SeqCst);
            Ok(RunResponse {
                artifacts: self
                    .outputs
                    .iter()
                    .map(|(group, name)| outputartifact::OutputArtifact {
                        group: group.clone(),
                        name: name.clone(),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                            data: b"hi".to_vec(),
                            path: name.clone(),
                            x: false,
                        }),
                        hashout: "feedface".to_string(),
                    })
                    .collect(),
                sandbox_cleanup: None,
                sandbox_guards: vec![],
            })
        }
        async fn run_shell<'a, 'io>(
            &self,
            _req: RunRequest<'a, 'io>,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<RunResponse> {
            anyhow::bail!("run_shell not supported by BlockingDriver")
        }
    }

    /// Provider serving exactly one `TargetSpec` (driven by `blocking`).
    struct OneTargetProvider {
        spec: TargetSpec,
    }

    impl crate::engine::provider::Provider for OneTargetProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "onetarget".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let spec = self.spec.clone();
            Box::pin(async move {
                if req.addr == spec.addr {
                    Ok(GetResponse { target_spec: spec })
                } else {
                    Err(GetError::NotFound)
                }
            })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    /// Engine with the `blocking` driver + a single cacheable target `//pkg:a`.
    /// Holds the cache/lock dirs in the returned `TempDir` (kept alive by caller).
    fn blocking_engine(
        exec_count: SArc<AtomicUsize>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Addr)> {
        blocking_engine_outputs(exec_count, vec![("main".to_string(), "out".to_string())])
    }

    /// Like [`blocking_engine`] but with a custom set of `(group, name)` outputs,
    /// for exercising multi-output / partial-cache behavior.
    fn blocking_engine_outputs(
        exec_count: SArc<AtomicUsize>,
        outputs: Vec<(String, String)>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Addr)> {
        let dir = tempdir()?;
        let mut engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            // sqlite-direct (no in-memory layer) so `list_target_entries` /
            // `delete` reflect writes synchronously — the partial-cache test
            // discovers the hashin and drops a blob by name deterministically.
            mem_cache: crate::engine::MemCacheOptions {
                capacity_bytes: 0,
                ..Default::default()
            },
            ..Default::default()
        })?;
        engine.register_driver(|_| {
            Box::new(BlockingDriver {
                exec_count,
                outputs: SArc::new(outputs),
            })
        })?;
        let addr = Addr::new(PkgBuf::from("pkg"), "a".to_string(), Default::default());
        let spec = TargetSpec {
            addr: addr.clone(),
            driver: "blocking".to_string(),
            config: HashMap::new(),
            labels: vec![],
            transitive: Default::default(),
        };
        engine.register_provider(move |_| Box::new(OneTargetProvider { spec }))?;
        Ok((Arc::new(engine), dir, addr))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_addr_two_output_variants_resolve_concurrently_completes() {
        // Reproduction. Two distinct `mem_result` cells for one addr — `All` and
        // `Exact["main"]` — both keep the produced artifact and so both hold a
        // riding read. On a cold cache the shared (memoized) meta wakes both at
        // once; both take coexisting shared reads, both miss, both then contend
        // the exclusive per-addr write. Pre-fix, the write-waiter blocks forever
        // on the sibling's riding read (single-process self-deadlock). Post-fix,
        // the addr-keyed `mem_locked_result` single-flight hands both callers one
        // shared read, so they complete.
        let exec_count = SArc::new(AtomicUsize::new(0));
        let (engine, _dir, addr) = blocking_engine(SArc::clone(&exec_count)).expect("engine");
        let rs = engine.new_state();

        let t1 = tokio::spawn(enclose!((engine, rs, addr) async move {
            engine
                .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
                .await
        }));
        let t2 = tokio::spawn(enclose!((engine, rs, addr) async move {
            engine
                .result_addr(
                    rs,
                    &addr,
                    OutputMatcher::Exact(vec!["main".to_string()]),
                    &ResultOptions::default(),
                )
                .await
        }));

        let (j1, j2) = tokio::time::timeout(Duration::from_secs(5), async {
            (t1.await.expect("t1 join"), t2.await.expect("t2 join"))
        })
        .await
        .expect("resolves must not self-deadlock on the per-addr result lock");

        let r1 = j1.expect("All variant resolves");
        let r2 = j2.expect("Exact[main] variant resolves");
        assert_eq!(r1.artifacts.len(), 1, "All must surface the 'main' output");
        assert_eq!(
            r2.artifacts.len(),
            1,
            "Exact[main] must surface the 'main' output"
        );
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "execute must be single-flighted across both output variants"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn distinct_requests_same_addr_share_cache_and_complete() {
        // The single-flight is per-`RequestStateData`: two separate requests get
        // separate `mem_locked_result` cells and still go through the real flock.
        // That's legitimate cross-request serialization (not the self-deadlock):
        // both complete, and the second hits the shared on-disk cache (keyed by
        // hashin), so execute runs exactly once across the two requests.
        let exec_count = SArc::new(AtomicUsize::new(0));
        let (engine, _dir, addr) = blocking_engine(SArc::clone(&exec_count)).expect("engine");

        let r1 = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("first request resolves");
        let r2 = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("second request resolves");

        assert_eq!(r1.artifacts.len(), 1);
        assert_eq!(r2.artifacts.len(), 1);
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "second request must hit the cross-request on-disk cache, not re-execute"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_hit_pulls_only_requested_output_not_every_group() {
        // Lazy-pull invariant: requesting one output group must NOT require the
        // other groups' blobs to be present. A target with two outputs (`main`,
        // `extra`) is built once; we then delete `extra`'s blob locally (the
        // manifest stays — modelling a partial/remote cache that only pulled
        // `main`). A fresh request for just `main` must hit the cache and return
        // it WITHOUT re-executing. The earlier all-outputs resolution would have
        // missed here (every group's blob required present) and re-run execute.
        let exec_count = SArc::new(AtomicUsize::new(0));
        let (engine, _dir, addr) = blocking_engine_outputs(
            SArc::clone(&exec_count),
            vec![
                ("main".to_string(), "out_main".to_string()),
                ("extra".to_string(), "out_extra".to_string()),
            ],
        )
        .expect("engine");

        // Build everything once (both blobs + manifest now cached locally).
        let built = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("initial build resolves");
        assert_eq!(built.artifacts.len(), 2, "All surfaces both output groups");
        assert_eq!(exec_count.load(Ordering::SeqCst), 1);

        // Drop the `extra` group's blob, keeping the manifest (partial cache).
        // Derive the cache key from `meta` (the input hash — computed from inputs,
        // no cache read) rather than enumerating the cache: the sqlite writer is
        // async and a fresh read connection may not yet see the just-written rows.
        let hashin = Arc::clone(&engine)
            .meta(engine.new_state(), &addr)
            .await
            .expect("meta")
            .hashin;
        engine
            .local_cache
            .delete(&addr, &hashin, "out_extra")
            .expect("delete extra blob");

        // Request only `main` in a fresh request: must hit, not re-execute.
        let r = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::Exact(vec!["main".to_string()]),
                &ResultOptions::default(),
            )
            .await
            .expect("Exact[main] resolves against the partial cache");
        assert_eq!(
            r.artifacts.len(),
            1,
            "only the requested 'main' is surfaced"
        );
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "requesting one output must not re-execute just because another \
             group's blob is absent — the pull stays lazy"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_hit_reads_manifest_once() {
        // T1.1: on a full cache hit the manifest must be read + deserialized
        // EXACTLY ONCE and reused for the per-caller output read. Pre-fix the
        // presence-probe and the per-caller read each parsed the manifest (two
        // backend reads per hit); now the probe stashes the parsed manifest on
        // `LockedResolution` and the caller filters its outputs from it.
        use crate::engine::local_cache::{LocalCache, MANIFEST_V1, SizedReader, TargetStream};

        struct CountingCache {
            inner: SArc<dyn LocalCache>,
            manifest_reads: SArc<AtomicUsize>,
        }
        impl LocalCache for CountingCache {
            fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<SizedReader> {
                if name == MANIFEST_V1 {
                    self.manifest_reads.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.reader(addr, hashin, name)
            }
            fn writer(
                &self,
                addr: &Addr,
                hashin: &str,
                name: &str,
            ) -> anyhow::Result<Box<dyn std::io::Write>> {
                self.inner.writer(addr, hashin, name)
            }
            fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool> {
                self.inner.exists(addr, hashin, name)
            }
            fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<()> {
                self.inner.delete(addr, hashin, name)
            }
            fn list_targets(&self) -> anyhow::Result<TargetStream> {
                self.inner.list_targets()
            }
            fn list_target_entries(&self, addr: &Addr) -> anyhow::Result<Vec<String>> {
                self.inner.list_target_entries(addr)
            }
            fn seekable_reader(
                &self,
                addr: &Addr,
                hashin: &str,
                name: &str,
            ) -> anyhow::Result<Option<Box<dyn hcore::hartifactcontent::ReadSeek + Send>>>
            {
                self.inner.seekable_reader(addr, hashin, name)
            }
        }

        let dir = tempdir().expect("tempdir");
        let mut engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            // sqlite-direct (no in-memory tier) so every manifest read reaches the
            // counter instead of being served from an LRU on the second touch.
            mem_cache: crate::engine::MemCacheOptions {
                capacity_bytes: 0,
                ..Default::default()
            },
            ..Default::default()
        })
        .expect("engine");
        let manifest_reads = SArc::new(AtomicUsize::new(0));
        engine.local_cache = SArc::new(CountingCache {
            inner: engine.local_cache.clone(),
            manifest_reads: SArc::clone(&manifest_reads),
        });
        let exec_count = SArc::new(AtomicUsize::new(0));
        engine
            .register_driver(enclose!(
                (exec_count) | _ | {
                    Box::new(BlockingDriver {
                        exec_count,
                        outputs: SArc::new(vec![("main".to_string(), "out".to_string())]),
                    })
                }
            ))
            .expect("driver");
        let addr = Addr::new(PkgBuf::from("pkg"), "a".to_string(), Default::default());
        let spec = TargetSpec {
            addr: addr.clone(),
            driver: "blocking".to_string(),
            config: HashMap::new(),
            labels: vec![],
            transitive: Default::default(),
        };
        engine
            .register_provider(move |_| Box::new(OneTargetProvider { spec }))
            .expect("provider");
        let engine = Arc::new(engine);

        // Cold build: writes the manifest + blob. (Miss path reads happen here;
        // we only assert on the subsequent hit.)
        let cold_rs = engine.new_state();
        engine
            .clone()
            .result_addr(
                cold_rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("initial build resolves");
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "cold build executes once"
        );

        // The cold build's cache write enqueues a fire-and-forget history-trim GC
        // on the background lane (see `try_trim_after_write`), which itself reads
        // the manifest. Drain it before resetting the counter so that lagging read
        // is never attributed to the hit below — otherwise the count races to 2.
        let pending = cold_rs.bg_pending();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pending.load(Ordering::SeqCst) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "background GC did not drain"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Fresh request → full cache hit. The manifest backing read must happen
        // exactly once for the whole resolution.
        manifest_reads.store(0, Ordering::SeqCst);
        let r = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("warm request hits the cache");
        assert_eq!(r.artifacts.len(), 1, "the single output is surfaced");
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "warm request must not re-execute"
        );
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            1,
            "the manifest must be read + parsed exactly once per cache hit, not twice"
        );
    }

    // ─── Codegen write-back / fixpoint / frozen ──────────────────────────────

    /// Engine wired with the exec driver AND the `@heph/fs` provider+driver, so
    /// the `//@heph/introspect:outputs` magic input resolves. Returns the engine
    /// and the workspace-root `TempDir` (which doubles as the home/cache root).
    fn engine_with_home_fs(
        targets: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        // `bash` driver wraps the `run` string into `bash -u -e -c <script>`,
        // so codegen targets can run real shell. The `@heph/fs` provider+driver
        // (auto-registered by `Engine::new`) resolves the synthesized
        // introspect-outputs inputs.
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_bash()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// A codegen target: `out` group `""` over `paths`, the given `codegen` mode
    /// (`"copy"`/`"in_place"`), depending on the magic introspect-outputs input
    /// so its declared output paths become `@heph/fs` inputs. Runs under the
    /// `bash` driver so `run` may be a shell script (cwd = sandbox `ws/<pkg>`).
    fn codegen_run_target(
        addr: &str,
        codegen: &str,
        paths: &[&str],
        run: &str,
    ) -> pluginstatictarget::Target {
        let mut out = HashMap::new();
        out.insert(
            "".to_string(),
            paths.iter().map(|s| s.to_string()).collect(),
        );
        let mut deps = HashMap::new();
        deps.insert(
            "".to_string(),
            vec!["//@heph/introspect:outputs".to_string()],
        );
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "bash".to_string(),
            run: Some(run.to_string()),
            out,
            codegen: Some(codegen.to_string()),
            deps,
            labels: vec![],
            ..Default::default()
        }
    }

    /// in_place does NOT restrict outputs to pre-existing inputs: a run that
    /// creates a net-new file matching its output glob succeeds and the file is
    /// written back to the tree. (The mode distinction is `copy` = generated /
    /// gitignored / glob-excluded vs `in_place` = committed / globbable — not
    /// "may only touch existing files".)
    #[tokio::test]
    async fn in_place_allows_net_new_files() -> anyhow::Result<()> {
        // out glob `pkg/*.txt`; nothing on disk → empty input set. The run script
        // (cwd = ws_dir/pkg) creates `created.txt`, a net-new output file.
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:gen",
            "in_place",
            &["*.txt"],
            "echo hi > created.txt",
        )])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:gen")?;

        let (res, _events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("in_place target may emit net-new files");
        assert_eq!(
            std::fs::read(root.path().join("pkg/created.txt"))?,
            b"hi\n",
            "net-new in_place output must be written back to the tree",
        );
        Ok(())
    }

    /// The on-disk exec bit is part of the `@heph/fs` (content + exec-bit) input
    /// hash, so the in_place write-back must apply the generated artifact's `x`
    /// even when the bytes are unchanged — otherwise a `+x`-only change never
    /// lands on disk and the recomputed fixpoint key would disagree with what ran.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_back_applies_exec_bit_on_unchanged_content() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:mkexec",
            "in_place",
            &["*.sh"],
            // Rewrite identical bytes, then mark the file executable in the
            // sandbox. The bytes match what's already on disk, so write-back
            // takes the unchanged path; only the exec bit differs and must be
            // reconciled onto the tree.
            "printf 'echo hi\\n' > run.sh && chmod +x run.sh",
        )])?;
        // Seed the exact bytes the run produces, but non-executable.
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        let script = pkg_dir.join("run.sh");
        std::fs::write(&script, b"echo hi\n")?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644))?;

        let addr = hmodel::htaddr::parse_addr("//pkg:mkexec")?;
        let (res, _events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("mkexec target resolves");

        let mode = std::fs::metadata(&script)?.permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "write-back must apply the exec bit even when content is unchanged (mode {mode:o})",
        );
        Ok(())
    }

    /// The headline guarantee: re-running an idempotent in_place transform over
    /// the already-transformed tree is a no-op cache hit (no re-execution).
    ///
    /// Run 1 normalizes a lowercase, newline-less source into uppercase-with-
    /// trailing-newline, writes it back, and registers a fixpoint cache revision
    /// keyed on the post-write-back tree state. Run 2 reads the now-normalized
    /// tree and must HIT the cache — the `fmt` target emits no `ExecuteStart`.
    /// (The `@heph/fs` *inputs* are still re-read each run, so we assert
    /// specifically that the `fmt` target did not execute.)
    ///
    /// The transform changes the file CONTENT on run 1 (`hello` → `HELLO\n`), so
    /// the fixpoint key provably differs from the primary key — `@heph/fs` hashes
    /// by (content, exec-bit), and the changed bytes alone separate the two keys.
    /// This makes the stored fixpoint revision observable (≥ 2 cache revisions).
    #[tokio::test]
    async fn fixpoint_hit() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:fmt",
            "in_place",
            // Package-relative; stored as `FilePath(pkg/in.txt)`.
            &["in.txt"],
            // cwd = ws/pkg. Uppercase + ensure exactly one trailing newline. The
            // command substitution strips trailing newlines, `printf '%s\n'`
            // re-adds one, so f(f(x)) == f(x); on the FIRST run over a newline-
            // less lowercase seed it also changes the content.
            "printf '%s\\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
        )])?;
        // Seed a lowercase, newline-less source → run 1 yields "HELLO\n", a
        // guaranteed content change (case + trailing newline).
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let fmt = addr.format();
        let fmt_executed = |evs: &[BuildEvent]| {
            evs.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if *addr == fmt),
            )
        };

        // Run 1: executes, writes back the normalized file, stores the fixpoint.
        let (first, ev1) = resolve_collecting_events(&engine, &addr).await;
        first.expect("first resolve must succeed");
        assert!(fmt_executed(&ev1), "first run must execute, got {ev1:?}");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"HELLO\n",
            "in_place run 1 must transform the tree file"
        );
        // Run 2: reads the already-transformed tree → fixpoint cache hit.
        // Because the transform changed the file CONTENT, run 2's hashin (over the
        // uppercased bytes) can only match the fixpoint revision, never the
        // primary (which is keyed over the lowercase seed). So "no execute on run
        // 2" is proof that the fixpoint revision was stored — no need to count
        // cache entries (the background GC may legitimately reclaim the now-stale
        // primary once the fixpoint exists).
        let (second, ev2) = resolve_collecting_events(&engine, &addr).await;
        second.expect("second resolve must succeed");
        assert!(
            !fmt_executed(&ev2),
            "second run over the transformed tree must be a cache hit (no execute), got {ev2:?}"
        );
        assert!(
            ev2.iter()
                .any(|e| matches!(&e.kind, BuildEventKind::LocalCacheHit { addr } if *addr == fmt)),
            "second run must record a local cache hit for fmt, got {ev2:?}"
        );
        // The tree is unchanged by the no-op second run.
        assert_eq!(std::fs::read(pkg_dir.join("in.txt"))?, b"HELLO\n");
        Ok(())
    }

    /// is_top gate: a codegen target reached only as a DEPENDENCY (not the
    /// directly-requested target) must NOT write its tree back. Locks the
    /// `is_top` memoizer-key fix — only the top-level frame materializes.
    #[tokio::test]
    async fn in_place_dep_is_not_written_back() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![
            codegen_run_target(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                "printf '%s\\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
            ),
            // A consumer that depends on the in_place target but is itself a
            // plain (non-codegen) target. Uses the `bash` driver registered by
            // engine_with_home_fs.
            pluginstatictarget::Target {
                addr: "//pkg:consumer".to_string(),
                driver: "bash".to_string(),
                run: Some("true".to_string()),
                out: HashMap::new(),
                codegen: None,
                deps: HashMap::from([("".to_string(), vec!["//pkg:fmt".to_string()])]),
                labels: vec![],
                ..Default::default()
            },
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;

        // Resolve the consumer (top-level). fmt is pulled in only as a dep
        // (is_top=false), so its tree must remain untouched.
        let consumer = hmodel::htaddr::parse_addr("//pkg:consumer")?;
        let (res, _ev) = resolve_collecting_events(&engine, &consumer).await;
        res.expect("consumer must resolve");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"hello",
            "an in_place target reached only as a dependency must NOT write its tree back"
        );

        // Sanity: requesting fmt directly DOES write it back.
        let fmt = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let (res, _ev) = resolve_collecting_events(&engine, &fmt).await;
        res.expect("fmt must resolve");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"HELLO\n",
            "a directly-requested in_place target must write its tree back"
        );
        Ok(())
    }

    /// Multi-file in_place: a glob output covering several files transforms each
    /// of them, and a re-run over the transformed tree is a no-op cache hit —
    /// exercising the per-file write-back walk and the multi-file fixpoint.
    #[tokio::test]
    async fn fixpoint_hit_multi_file() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:fmt",
            "in_place",
            &["*.txt"],
            // cwd = ws/pkg. Normalize every .txt file in place.
            "for f in *.txt; do printf '%s\\n' \"$(tr a-z A-Z < \"$f\")\" > \"$f.t\" && mv \"$f.t\" \"$f\"; done",
        )])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("a.txt"), b"aaa")?;
        std::fs::write(pkg_dir.join("b.txt"), b"bbb")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let fmt = addr.format();
        let fmt_executed = |evs: &[BuildEvent]| {
            evs.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if *addr == fmt),
            )
        };

        let (first, ev1) = resolve_collecting_events(&engine, &addr).await;
        first.expect("first resolve must succeed");
        assert!(fmt_executed(&ev1), "first run must execute");
        assert_eq!(std::fs::read(pkg_dir.join("a.txt"))?, b"AAA\n");
        assert_eq!(std::fs::read(pkg_dir.join("b.txt"))?, b"BBB\n");

        let (second, ev2) = resolve_collecting_events(&engine, &addr).await;
        second.expect("second resolve must succeed");
        assert!(
            !fmt_executed(&ev2),
            "second run over the transformed multi-file tree must be a cache hit, got {ev2:?}"
        );
        Ok(())
    }

    /// End-to-end provenance: after a `copy` codegen target writes+stamps a
    /// net-new file, a subsequent `@heph/fs` glob over the same tree EXCLUDES it
    /// (so it is never double-sourced), while an unstamped in_place output stays
    /// visible to a glob.
    #[tokio::test]
    async fn stamped_copy_output_excluded_from_later_glob() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![
            codegen_run_target("//pkg:cp", "copy", &["*.gen"], "echo generated > out.gen"),
            codegen_run_target("//pkg:ip", "in_place", &["keep.txt"], "true"),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("keep.txt"), b"keep\n")?;

        // Skip the exclusion assertions on a filesystem that can't persist xattrs.
        let probe = root.path().join(".xattr_probe");
        std::fs::write(&probe, b"x")?;
        let xattr_supported = xattr::set(&probe, hbuiltins::pluginfs::CODEGEN_XATTR, b"v").is_ok();

        // Materialize + stamp the copy output, and write back the in_place file.
        resolve_collecting_events(&engine, &hmodel::htaddr::parse_addr("//pkg:cp")?)
            .await
            .0
            .expect("copy target resolves");
        resolve_collecting_events(&engine, &hmodel::htaddr::parse_addr("//pkg:ip")?)
            .await
            .0
            .expect("in_place target resolves");

        // A glob over the stamped copy output must yield nothing.
        let gen_glob = hbuiltins::pluginfs::glob_addr("pkg/*.gen", &[]);
        let (res, _) = resolve_collecting_events(&engine, &gen_glob).await;
        let gen_res = res.expect("glob over generated files resolves");
        // A glob over the unstamped in_place output must still see it.
        let keep_glob = hbuiltins::pluginfs::glob_addr("pkg/keep.txt", &[]);
        let (res, _) = resolve_collecting_events(&engine, &keep_glob).await;
        let keep_res = res.expect("glob over in_place output resolves");

        if xattr_supported {
            assert!(
                gen_res.artifacts.is_empty(),
                "stamped copy output must be excluded from a later glob, got {} artifacts",
                gen_res.artifacts.len(),
            );
        }
        assert!(
            !keep_res.artifacts.is_empty(),
            "unstamped in_place output must remain visible to a glob",
        );
        Ok(())
    }

    /// A net-new `copy` codegen target: after resolve the generated file is
    /// materialized into the workspace root AND carries the codegen xattr (so a
    /// later fs glob excludes it). An in_place target's re-emitted file exists
    /// but is NOT stamped.
    #[tokio::test]
    async fn writeback_xattr() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![
            // Copy: generates a net-new file. The introspect input is a glob
            // (`pkg/*.gen`) so the not-yet-existing output doesn't error at
            // input resolution the way a `file()` over a missing path would.
            codegen_run_target("//pkg:cp", "copy", &["*.gen"], "echo generated > out.gen"),
            // In-place: re-emits an existing tracked source file untouched.
            codegen_run_target("//pkg:ip", "in_place", &["src.txt"], "true"),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("src.txt"), b"src\n")?;

        // Probe whether this filesystem actually persists xattrs; skip the
        // assertions (not the run) if not, so a tmpfs can't make us flake.
        let probe = root.path().join(".xattr_probe");
        std::fs::write(&probe, b"x")?;
        let xattr_supported = xattr::set(&probe, hbuiltins::pluginfs::CODEGEN_XATTR, b"v").is_ok();

        let cp_addr = hmodel::htaddr::parse_addr("//pkg:cp")?;
        let ip_addr = hmodel::htaddr::parse_addr("//pkg:ip")?;

        let (res, _e) = resolve_collecting_events(&engine, &cp_addr).await;
        res.expect("copy codegen target must resolve");
        let (res, _e) = resolve_collecting_events(&engine, &ip_addr).await;
        res.expect("in_place codegen target must resolve");

        let gen_file = root.path().join("pkg/out.gen");
        assert!(
            gen_file.exists(),
            "copy codegen file must be written to the tree"
        );
        let src_file = root.path().join("pkg/src.txt");
        assert!(
            src_file.exists(),
            "in_place codegen file must exist in the tree"
        );

        if xattr_supported {
            assert!(
                xattr::get(&gen_file, hbuiltins::pluginfs::CODEGEN_XATTR)?.is_some(),
                "net-new copy output must carry the codegen xattr"
            );
            assert!(
                xattr::get(&src_file, hbuiltins::pluginfs::CODEGEN_XATTR)?.is_none(),
                "in_place output must NOT carry the codegen xattr"
            );
        }
        Ok(())
    }

    /// `--frozen` on an in_place fmt target: when the tree does not yet match the
    /// generated output, the check fails with a typed `FrozenCheckError` and
    /// nothing is written; once the tree matches, it succeeds.
    #[tokio::test]
    async fn frozen_fails_on_dirty() -> anyhow::Result<()> {
        // The run script normalizes the input (uppercases) in place, so the
        // generated output differs from a lowercase tree file but matches an
        // already-uppercase one. Each scenario uses its OWN engine/root and runs
        // the target exactly once — avoiding the sandbox reuse that a second
        // execute on the same no-args addr would trigger.
        let run = "tr a-z A-Z < in.txt > in.txt.tmp && mv in.txt.tmp in.txt";
        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let frozen_opts = ResultOptions {
            frozen: true,
            ..Default::default()
        };

        let seed_engine = |seed: &'static [u8]| {
            let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                run,
            )])?;
            let pkg_dir = root.path().join("pkg");
            std::fs::create_dir_all(&pkg_dir)?;
            std::fs::write(pkg_dir.join("in.txt"), seed)?;
            anyhow::Ok((engine, root))
        };

        // Dirty tree (lowercase): frozen must fail with a typed FrozenCheckError
        // and write nothing.
        let (engine, root) = seed_engine(b"hello\n")?;
        let tree_file = root.path().join("pkg/in.txt");
        let rs = engine.new_state();
        let err = engine
            .clone()
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &frozen_opts)
            .await
            .err()
            .expect("frozen check on a dirty tree must error");
        drop(rs);
        // The top-level frame surfaces the recorded `TargetFailure` whose `source`
        // anyhow chain carries the original `FrozenCheckError`.
        let tf = err
            .downcast_ref::<TargetFailure>()
            .expect("top-level error must be a recorded TargetFailure");
        assert!(
            downcast_chain_ref::<crate::engine::error::FrozenCheckError>(tf.source.as_ref())
                .is_some(),
            "frozen failure must carry a FrozenCheckError, got: {err:#}"
        );
        assert_eq!(
            std::fs::read(&tree_file)?,
            b"hello\n",
            "frozen mode must not modify the tree"
        );

        // Clean tree (already uppercase): frozen must pass.
        let (engine, _root) = seed_engine(b"HELLO\n")?;
        let rs = engine.new_state();
        engine
            .clone()
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &frozen_opts)
            .await
            .expect("frozen check on a clean tree must succeed");
        Ok(())
    }

    /// `--frozen` must also catch an exec-bit-only divergence: the on-disk bytes
    /// match the generated output but the exec bit differs. Since `@heph/fs` now
    /// hashes (content + exec-bit), this is real drift a non-frozen run would
    /// write back, so frozen must fail rather than report the tree clean.
    #[cfg(unix)]
    #[tokio::test]
    async fn frozen_fails_on_exec_bit_drift() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let addr = hmodel::htaddr::parse_addr("//pkg:mkexec")?;
        let frozen_opts = ResultOptions {
            frozen: true,
            ..Default::default()
        };
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:mkexec",
            "in_place",
            &["*.sh"],
            // Identical bytes to the seed, but executable in the sandbox.
            "printf 'echo hi\\n' > run.sh && chmod +x run.sh",
        )])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        let script = pkg_dir.join("run.sh");
        std::fs::write(&script, b"echo hi\n")?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644))?;

        let rs = engine.new_state();
        let err = engine
            .clone()
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &frozen_opts)
            .await
            .err()
            .expect("frozen check must fail on exec-bit-only drift");
        drop(rs);
        let tf = err
            .downcast_ref::<TargetFailure>()
            .expect("top-level error must be a recorded TargetFailure");
        assert!(
            downcast_chain_ref::<crate::engine::error::FrozenCheckError>(tf.source.as_ref())
                .is_some(),
            "frozen failure must carry a FrozenCheckError, got: {err:#}"
        );
        // Frozen never writes: the tree file stays non-executable.
        let mode = std::fs::metadata(&script)?.permissions().mode();
        assert!(
            mode & 0o111 == 0,
            "frozen mode must not chmod the tree (mode {mode:o})"
        );
        Ok(())
    }

    /// Mirror of `write_back_applies_exec_bit_on_unchanged_content` for the strip
    /// direction: an in_place target that emits a NON-executable file over a
    /// byte-identical executable tree file must clear the exec bit on write-back
    /// (x=false is part of the hash identity just as x=true is).
    #[cfg(unix)]
    #[tokio::test]
    async fn write_back_strips_exec_bit_on_unchanged_content() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:rmexec",
            "in_place",
            &["*.sh"],
            // Identical bytes, but explicitly non-executable in the sandbox.
            "printf 'echo hi\\n' > run.sh && chmod -x run.sh",
        )])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        let script = pkg_dir.join("run.sh");
        std::fs::write(&script, b"echo hi\n")?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;

        let addr = hmodel::htaddr::parse_addr("//pkg:rmexec")?;
        let (res, _events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("rmexec target resolves");

        let mode = std::fs::metadata(&script)?.permissions().mode();
        assert!(
            mode & 0o111 == 0,
            "write-back must strip the exec bit even when content is unchanged (mode {mode:o})",
        );
        Ok(())
    }

    /// A codegen target with MULTIPLE output groups must write back EVERY group's
    /// artifact. The write-back loop is driven off the cached artifacts (looking
    /// up each group's mode), so it covers all of them — not just the first one
    /// found for a group.
    #[tokio::test]
    async fn multi_group_codegen_writes_all_groups() -> anyhow::Result<()> {
        let target = pluginstatictarget::Target {
            addr: "//pkg:fmt".to_string(),
            driver: "bash".to_string(),
            run: Some(
                "tr a-z A-Z < a.txt > a.t && mv a.t a.txt; \
                 tr a-z A-Z < b.txt > b.t && mv b.t b.txt"
                    .to_string(),
            ),
            out: HashMap::from([
                ("ga".to_string(), vec!["a.txt".to_string()]),
                ("gb".to_string(), vec!["b.txt".to_string()]),
            ]),
            codegen: Some("in_place".to_string()),
            deps: HashMap::from([(
                "".to_string(),
                vec!["//@heph/introspect:outputs".to_string()],
            )]),
            labels: vec![],
            ..Default::default()
        };
        let (engine, root) = engine_with_home_fs(vec![target])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("a.txt"), b"aaa\n")?;
        std::fs::write(pkg_dir.join("b.txt"), b"bbb\n")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        resolve_collecting_events(&engine, &addr)
            .await
            .0
            .expect("multi-group codegen target must resolve");

        assert_eq!(
            std::fs::read(pkg_dir.join("a.txt"))?,
            b"AAA\n",
            "output group `ga` must be written back",
        );
        assert_eq!(
            std::fs::read(pkg_dir.join("b.txt"))?,
            b"BBB\n",
            "output group `gb` must be written back",
        );
        Ok(())
    }

    /// in_place targets persist two cache revisions per state (primary +
    /// fixpoint), so their GC `history` is doubled at eval time (on the def both
    /// GC paths read). copy / plain targets keep their declared history.
    #[tokio::test]
    async fn in_place_doubles_gc_history() -> anyhow::Result<()> {
        let (engine, _root) = engine_with_home_fs(vec![
            codegen_run_target("//pkg:fmt", "in_place", &["in.txt"], "true"),
            codegen_run_target("//pkg:gen", "copy", &["*.gen"], "echo x > out.gen"),
        ])?;
        let rs = engine.new_state();

        let fmt = Arc::clone(&engine)
            .get_def(rs.clone(), &hmodel::htaddr::parse_addr("//pkg:fmt")?)
            .await?;
        assert_eq!(
            fmt.target_def.cache.history, 2,
            "in_place target must double the default history (1 → 2)",
        );

        let gen_def = Arc::clone(&engine)
            .get_def(rs.clone(), &hmodel::htaddr::parse_addr("//pkg:gen")?)
            .await?;
        assert_eq!(
            gen_def.target_def.cache.history, 1,
            "copy target keeps its declared history",
        );
        Ok(())
    }
}
