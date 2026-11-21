//! heph's [`LspContext`] implementation: it teaches `starlark_lsp` about heph's
//! globals (so the builtins complete and hover), resolves `load()` paths and
//! target-address string literals, completes target addresses, and — as a side
//! effect of evaluating each opened buffer — populates the per-document index the
//! proxy uses for the provenance hover and driver-schema completion.

use super::index::{DocIndex, SharedState};
#[cfg(any(test, feature = "test-support"))]
use crate::pluginbuildfile::Provider;
use crate::pluginbuildfile::provider::PackageList;
use crate::pluginbuildfile::run_file::{
    BuildFileLoader, build_globals, eval_source, resolve_load_target,
};
use hplugin::function::FunctionRegistry;
pub(crate) use hplugin::lsp::BadPath;
use hplugin::lsp::{Listed, Listing, LspEngine, check_path};
use starlark::docs::DocModule;
use starlark::environment::Globals;
use starlark::errors::EvalMessage;
use starlark::syntax::{AstModule, Dialect};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use vendored_starlark_lsp::completion::{StringCompletionResult, StringCompletionType};
use vendored_starlark_lsp::error::eval_message_to_lsp_diagnostic;
use vendored_starlark_lsp::server::{LspContext, LspEvalResult, LspUri, StringLiteralResult};
use vendored_starlark_lsp::uri::file_path_to_uri;

pub(crate) struct HephLspContext {
    root: PathBuf,
    registry: Arc<FunctionRegistry>,
    /// Docs for the heph globals; drives `starlark_lsp`'s builtin completion/hover.
    doc_globals: DocModule,
    /// Lazily-built Starlark globals, shared across per-buffer evaluations.
    globals: Arc<OnceLock<Globals>>,
    /// BUILD-file name patterns, from the buildfile provider's `patterns` config
    /// option (defaulting to `BUILD`). Used for both listing and load resolution.
    patterns: Vec<glob::Pattern>,
    /// The runtime the engine's listing and the buffer evaluation spawn on —
    /// captured at construction (the LSP context is built on the server's
    /// runtime), so sync callers don't have to discover one.
    runtime: tokio::runtime::Handle,
    /// How long a completion waits for the engine's listing before it
    /// answers stock ([`LISTING_WAIT`] unless a test shortens it).
    listing_wait: Duration,
    pub(crate) shared: Arc<SharedState>,
}

/// How long a completion waits for the engine's listing. Past it, the
/// completion answers stock with `reason: listing_timed_out`, and the
/// listing is dropped: the engine cancels its request, so its package tasks
/// stop and its scope ends.
pub(crate) const LISTING_WAIT: Duration = Duration::from_secs(2);

/// Why a completion got no listing.
#[derive(Debug)]
pub(crate) enum ListingError {
    /// The listing did not answer within the wait.
    TimedOut(Duration),
    /// The engine's listing returned an error.
    Failed(anyhow::Error),
}

impl HephLspContext {
    pub(crate) fn new(engine: Arc<dyn LspEngine>, listing_wait: Duration) -> HephLspContext {
        let root = engine.root().to_path_buf();
        let registry = engine.function_registry();
        let doc_globals = build_globals(&registry).documentation();
        let core_members = crate::pluginbuildfile::run_file::heph_core_members(&doc_globals);
        let builtin_hovers = crate::pluginbuildfile::run_file::BuiltinHovers::new(&doc_globals);
        let patterns = buildfile_patterns(&*engine);
        let runtime = tokio::runtime::Handle::current();
        let shared = SharedState::new(
            engine,
            root.clone(),
            patterns.clone(),
            core_members,
            builtin_hovers,
        );
        HephLspContext {
            root,
            registry,
            doc_globals,
            globals: Arc::new(OnceLock::new()),
            patterns,
            runtime,
            listing_wait,
            shared,
        }
    }

    /// The documentation of the global `name` (a builtin), if it has any.
    pub(crate) fn doc_global(&self, name: &str) -> Option<&starlark::docs::DocItem> {
        self.doc_globals.members.get(name)
    }

    /// Package name for a BUILD file path (its parent dir, workspace-relative,
    /// forward-slashed). Empty string for a BUILD file at the workspace root.
    fn path_to_pkg(&self, path: &Path) -> String {
        let parent = path.parent().unwrap_or(path);
        let rel = parent.strip_prefix(&self.root).unwrap_or(parent);
        rel.to_string_lossy().replace('\\', "/")
    }

    /// The 0-based line in `file` of the outermost call that declared target
    /// `name` when `text` (that file's contents) is evaluated as package
    /// `pkg`: `tool("go")` for a target a helper declares, the `target(` call
    /// for a direct one. `None` when the file does not evaluate or declares no
    /// such target. Evaluation only enriches the answer: the caller falls back
    /// to reading the tokens.
    pub(crate) fn declaring_line(
        &self,
        file: &Path,
        text: String,
        pkg: &str,
        name: &str,
    ) -> Option<u32> {
        let filename = file.to_string_lossy();
        // A helper loaded from another file may reach the walk, which needs a
        // runtime context (see `listing`).
        let _rt = self.runtime.enter();
        let result = eval_source(&filename, text, pkg, &self.loader()).ok()?;
        let target = result.targets.iter().find(|t| t.name == name)?;
        // The call stack is outermost first, so the first frame in this file
        // is the call the package itself made.
        target
            .provenance
            .iter()
            .find(|f| f.file == filename)
            .map(|f| f.line_start.saturating_sub(1))
    }

    fn loader(&self) -> BuildFileLoader {
        let walker = Arc::new(hwalk::CachedWalker::disabled());
        BuildFileLoader::new(
            self.root.clone(),
            self.patterns.clone(),
            // Fresh per-call caches: the open buffer may differ from disk, so the
            // main file must never be served from a stale cache.
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::clone(&self.registry),
            Arc::clone(&self.globals),
            Arc::clone(&walker),
            // Fresh package list per call, for the same reason: a session-lifetime
            // one would freeze editor results — create a package and it would
            // never appear. Within one buffer evaluation it still dedupes the
            // walk. Note this passes only the always-on `.git` prune where the
            // build passes the configured `fs.skip`, so the editor can see
            // packages the build does not; a pre-existing divergence, now at least
            // confined to a cell that is never shared with the build's.
            //
            // `.git` specifically must be pruned, not merely skipped as an
            // optimization: a ref file's name is the branch name's raw bytes, so
            // one `git checkout -b $'\xe9'` puts a name in `.git/refs/heads` that
            // the walker refuses. Walking `.git` would fail the whole package
            // list — and with it every completion — over something no build
            // would ever have read.
            Arc::new(PackageList::new(
                self.root.clone(),
                self.patterns.clone(),
                Arc::new(hwalk::Ignore::git_only()),
                walker,
            )),
            Arc::default(),
        )
    }

    /// What the engine lists for `what`, in a cache scope of its own (see
    /// [`LspEngine::list`]), waiting at most the listing wait for it. Past
    /// the wait the listing is dropped, which cancels its walk and ends its
    /// scope. Either failure is logged here, and the server also tells the
    /// editor (`window/logMessage`), since it is the only explanation a stock
    /// answer at an address will ever get.
    pub(crate) fn listing(&self, what: Listing) -> Result<Listed, ListingError> {
        let fut = Arc::clone(&self.shared.engine).list(what.clone());
        // Entered, not spawned: this runs on the LSP server's sync request
        // thread, which has no runtime context of its own, and the listing
        // spawns package tasks and reaches `hcore::blocking::run`, which need
        // one. Entering the captured server runtime satisfies them; the wait
        // itself parks only this thread.
        // `listing_works_from_a_thread_with_no_ambient_runtime` pins it.
        let _rt = self.runtime.enter();
        match block_on_timeout(fut, self.listing_wait) {
            Some(Ok(listed)) => {
                for gap in &listed.skipped {
                    tracing::debug!(
                        scope = listed.scope.get(),
                        stage = gap.stage,
                        provider = gap.provider,
                        count = gap.count,
                        examples = ?gap.examples,
                        "lsp: listing left out"
                    );
                }
                Ok(listed)
            }
            Some(Err(e)) => {
                tracing::warn!(error = ?e, ?what, "lsp: listing failed");
                Err(ListingError::Failed(e))
            }
            None => {
                tracing::warn!(
                    waited_ms = self.listing_wait.as_millis() as u64,
                    ?what,
                    "lsp: listing timed out"
                );
                Err(ListingError::TimedOut(self.listing_wait))
            }
        }
    }

    /// The on-disk BUILD file URL for a package.
    fn build_file_url(&self, package: &str) -> Option<LspUri> {
        let path = first_build_file(&self.root.join(package), &self.patterns)?;
        file_path_to_uri(&path).and_then(|u| LspUri::try_from(u).ok())
    }
}

/// First file in `dir` whose name matches one of `patterns` (handles literal
/// names like `BUILD` and globs like `*.BUILD`). Used to resolve a package /
/// `load("//pkg", …)` directory to its actual BUILD file.
fn first_build_file(dir: &Path, patterns: &[glob::Pattern]) -> Option<PathBuf> {
    crate::pluginbuildfile::provider::build_files_in_dir(dir, patterns)
        .into_iter()
        .next()
}

/// The listing that holds every package starting with `prefix`: the packages
/// under its last whole path segment (`foo/ba` → under `foo`, `@heph/go/g` →
/// under `@heph/go`, `fo` → under the root). The engine's package matcher
/// works in whole segments, so the partial one is the caller's filter.
///
/// `prefix` is typed text and the path reaches every provider, some of which
/// join it to the workspace root and walk the result, so a prefix that does
/// not name a place inside the workspace is refused ([`check_path`], which
/// the engine runs again on every listing it is handed). The
/// partial segment only filters, so only its bytes are checked.
pub(crate) fn packages_under(prefix: &str) -> Result<Listing, BadPath> {
    let under = prefix.rsplit_once('/').map_or("", |(dir, _)| dir);
    check_path(prefix, under)?;
    Ok(Listing::Packages {
        under: under.to_string(),
    })
}

/// The listing of `package`'s targets; `package` is checked as
/// [`packages_under`] checks a prefix, every segment of it whole.
pub(crate) fn targets_in(package: &str) -> Result<Listing, BadPath> {
    let listing = Listing::Targets {
        package: package.to_string(),
    };
    listing.check()?;
    Ok(listing)
}

/// Polls `fut` on this thread until it is ready or `wait` has passed;
/// `None` on the latter, with `fut` dropped. `futures::executor::block_on`
/// with a deadline, so no runtime timer is involved: the thread parks until
/// the future wakes it or the deadline comes.
pub(crate) fn block_on_timeout<F: std::future::Future>(
    fut: F,
    wait: Duration,
) -> Option<F::Output> {
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let deadline = Instant::now() + wait;
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return Some(out);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        // A spurious return only costs one more poll.
        std::thread::park_timeout(left);
    }
}

/// A test engine's [`LspEngine::list`]: a fresh buildfile provider over
/// `root` per call, which is what a fresh cache scope gives the real one.
/// Every entry is attributed to `buildfile`; no provider is told the scope
/// ended. The scope is a real, distinct id that no provider ever sees.
#[cfg(any(test, feature = "test-support"))]
pub fn list_with_fresh_buildfile_provider(
    root: PathBuf,
    registry: Arc<FunctionRegistry>,
    what: Listing,
) -> futures::future::BoxFuture<'static, anyhow::Result<Listed>> {
    use hcore::hasync::StdCancellationToken;
    use hmodel::htpkg::PkgBuf;
    use hplugin::provider::{ListPackagesRequest, ListRequest, Provider as _};
    Box::pin(async move {
        let provider = Provider {
            root,
            build_file_patterns: crate::pluginbuildfile::provider::default_build_file_patterns(),
            ..Provider::base(tokio::runtime::Handle::current())
        }
        .with_function_registry(registry);
        let ctoken = StdCancellationToken::new();
        let mut skipped = Vec::new();
        let mut names: Vec<String> = match &what {
            Listing::Packages { under } => provider
                .list_packages(
                    ListPackagesRequest::new("lsp-test", PkgBuf::from(under.as_str())),
                    &ctoken,
                )
                .await?
                .filter_map(Result::ok)
                .map(|r| r.pkg.to_string())
                .filter(|p| {
                    under.is_empty()
                        || p == under
                        || p.strip_prefix(under.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
                .collect(),
            // A package that does not evaluate costs only its own entries and
            // is reported, as `Discovery::KeepGoing` makes it in the engine.
            Listing::Targets { package } => match provider
                .list(
                    ListRequest::new(
                        "lsp-test",
                        PkgBuf::from(package.as_str()),
                        vec![],
                        Arc::new(hplugin::provider::NoopExecutor),
                    ),
                    &ctoken,
                )
                .await
            {
                Ok(listed) => listed
                    .filter_map(Result::ok)
                    .map(|r| r.addr.name.clone())
                    .collect(),
                Err(e) => {
                    skipped.push(hcore::events::SelectionGap {
                        stage: "list".to_string(),
                        provider: "buildfile".to_string(),
                        count: 1,
                        examples: vec![hcore::events::SelectionGapExample {
                            scope: format!("//{package}"),
                            cause: format!("{e:#}"),
                        }],
                    });
                    Vec::new()
                }
            },
        };
        names.sort_unstable();
        names.dedup();
        let buildfile: Arc<str> = Arc::from("buildfile");
        Ok(Listed {
            scope: hplugin::scope::CacheScope::mint(|_, _| {}).id(),
            entries: names
                .into_iter()
                .map(|name| hplugin::lsp::ListedEntry {
                    name,
                    provider: Arc::clone(&buildfile),
                })
                .collect(),
            skipped,
            scope_ended: Box::pin(async { Vec::new() }),
        })
    })
}

/// BUILD-file name patterns from the workspace's buildfile-provider config,
/// mirroring what the engine's provider uses. Falls back to the shared defaults
/// when the config is absent, lists no patterns, or none compile.
fn buildfile_patterns(engine: &dyn LspEngine) -> Vec<glob::Pattern> {
    let opts = engine.provider_options("buildfile");
    crate::pluginbuildfile::provider::build_file_patterns_from_options(&opts)
        .unwrap_or_else(|_| crate::pluginbuildfile::provider::default_build_file_patterns())
}

impl LspContext for HephLspContext {
    fn parse_file_with_contents(&self, uri: &LspUri, content: String) -> LspEvalResult {
        let path = match uri {
            LspUri::File(p) | LspUri::Starlark(p) => p.clone(),
            _ => return LspEvalResult::default(),
        };
        let filename = path.to_string_lossy().to_string();
        // An open document's parse comes with the tokens the loop just built;
        // a file read from disk for the stock analysis (a loaded file, say)
        // comes without, and gets an AST and nothing else: no index, no
        // overlay, no evaluation, so memory stays bounded by open documents.
        let staged = self.shared.take_tokens(uri);

        // Parse the AST for the stock analysis (symbols, hover, goto); parse
        // errors become diagnostics.
        let ast = match AstModule::parse(&filename, content.clone(), &Dialect::Extended) {
            Ok(ast) => ast,
            Err(e) => {
                if staged.is_some() {
                    self.shared
                        .set_index(uri.clone(), DocIndex::unevaluated(content));
                }
                return LspEvalResult {
                    diagnostics: vec![eval_message_to_lsp_diagnostic(EvalMessage::from_error(
                        &path, &e,
                    ))],
                    ast: None,
                };
            }
        };
        let Some(tokens) = staged else {
            return LspEvalResult {
                diagnostics: Vec::new(),
                ast: Some(ast),
            };
        };

        // The name sets the resolver overlays on the tokens (D3): kept until
        // the next good parse replaces them.
        let overlay = super::site::overlay_of(&ast);

        // Static validation of `target` / `provider_state` keyword arguments
        // against the engine's schemas (unknown key / wrong type → red squiggle).
        // Runs off the parsed AST, so it works mid-edit without a full eval;
        // shadowing and driver selection come from the resolver's helpers.
        let doc = super::site::Doc {
            text: &content,
            tokens: &tokens,
            overlay: Some(&overlay),
            ast: super::site::AstState::Current,
        };
        let diagnostics = super::diagnostics::validate(&ast, &doc, &*self.shared.engine);
        self.shared.set_overlay(uri.clone(), overlay);

        // Best-effort evaluation to populate the provenance / driver index. A
        // half-typed buffer (or one the engine rejects — an unknown key on a
        // macro call fails the whole evaluation) simply yields no provenance;
        // the source and its `load(...)` imports survive, so completion, hover
        // and goto-definition keep working. We do not surface eval errors as
        // diagnostics (they would be noisy while typing).
        let pkg = self.path_to_pkg(&path);
        let source = content.clone();
        if let Ok(result) = eval_source(&filename, content, &pkg, &self.loader()) {
            self.shared
                .set_index(uri.clone(), DocIndex::build(&result, &pkg, source));
        } else {
            self.shared
                .set_index(uri.clone(), DocIndex::unevaluated(source));
        }

        LspEvalResult {
            diagnostics,
            ast: Some(ast),
        }
    }

    fn resolve_load(
        &self,
        path: &str,
        current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<LspUri, String> {
        // Resolve exactly as the buildfile loader does (`//pkg`, `//pkg/x.BUILD`,
        // `./rel`, `../rel`), so loaded symbols point at the same file heph reads.
        let current_pkg = match current_file {
            LspUri::File(p) => self.path_to_pkg(p),
            _ => String::new(),
        };
        let resolved =
            resolve_load_target(&self.root, &current_pkg, path).map_err(|e| e.to_string())?;
        // `load("//pkg", …)` targets a directory: resolve to its BUILD file so the
        // file can be parsed for its exported symbols.
        let file = if resolved.is_dir() {
            first_build_file(&resolved, &self.patterns)
                .ok_or_else(|| format!("no BUILD file in {}", resolved.display()))?
        } else {
            resolved
        };
        let uri = file_path_to_uri(&file).ok_or_else(|| "non-file load path".to_string())?;
        LspUri::try_from(uri).map_err(|e| e.to_string())
    }

    fn render_as_load(
        &self,
        target: &LspUri,
        _current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<String, String> {
        match target {
            LspUri::File(target_path) => {
                let pkg = self.path_to_pkg(target_path);
                let file = target_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                Ok(format!("//{pkg}:{file}"))
            }
            _ => Err("can only render file:// load paths".to_string()),
        }
    }

    fn resolve_string_literal(
        &self,
        literal: &str,
        current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<Option<StringLiteralResult>, String> {
        // Goto-definition on a target address: `//pkg:name` (absolute) or `:name`
        // (relative to the current file's package). Jump to the package's BUILD
        // file. Non-address strings resolve to nothing.
        let package = if let Some(rest) = literal.strip_prefix("//") {
            rest.split_once(':').map(|(p, _)| p.to_string())
        } else if let Some(_name) = literal.strip_prefix(':') {
            match current_file {
                LspUri::File(p) => Some(self.path_to_pkg(p)),
                _ => None,
            }
        } else {
            None
        };

        Ok(package.and_then(|pkg| {
            self.build_file_url(&pkg).map(|uri| StringLiteralResult {
                uri,
                location_finder: None,
            })
        }))
    }

    fn get_load_contents(&self, uri: &LspUri) -> Result<Option<String>, String> {
        match uri {
            LspUri::File(p) => Ok(std::fs::read_to_string(p).ok()),
            _ => Ok(None),
        }
    }

    fn get_environment(&self, _uri: &LspUri) -> DocModule {
        self.doc_globals.clone()
    }

    fn get_uri_for_global_symbol(
        &self,
        _current_file: &LspUri,
        _symbol: &str,
    ) -> Result<Option<LspUri>, String> {
        Ok(None)
    }

    fn get_string_completion_options(
        &self,
        _document_uri: &LspUri,
        _kind: StringCompletionType,
        _current_value: &str,
        _workspace_root: Option<&Path>,
    ) -> Result<Vec<StringCompletionResult>, String> {
        // Every string the stock analysis completes (a load path, an
        // address) is a heph `load_path` / `address` site, answered from the
        // site's one listing choice (`handlers::listing_query`). The stock
        // answer is reached there only when heph steps aside (outside the
        // root, a listing that timed out or failed), and listing again then
        // would be a second classifier of the string, and a second wait.
        Ok(Vec::new())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::{
        BadPath, HephLspContext, LISTING_WAIT, ListingError, LspContext as _, first_build_file,
        packages_under, targets_in,
    };
    use hplugin::config::Options;
    use hplugin::driver::DriverSchema;
    use hplugin::function::FunctionRegistry;
    use hplugin::lsp::{Listing, LspEngine};
    use hplugin::provider::StateSchema;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// How a [`FakeEngine`]'s `list` answers.
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) enum ListMode {
        /// A fresh buildfile provider over the root, as a fresh scope gives.
        #[default]
        Fresh,
        /// An error, as an engine whose listing failed returns.
        Fails,
        /// Never, as a provider that hangs.
        Hangs,
        /// A fresh listing whose scope never ends.
        ScopeHeld,
    }

    /// Minimal `LspEngine` over a workspace root: enough for the listing
    /// provider, which is all the walk path needs.
    pub(crate) struct FakeEngine {
        pub(crate) root: PathBuf,
        pub(crate) list: ListMode,
    }

    impl FakeEngine {
        pub(crate) fn new(root: &Path) -> FakeEngine {
            FakeEngine {
                root: root.to_path_buf(),
                list: ListMode::Fresh,
            }
        }
    }

    impl LspEngine for FakeEngine {
        fn root(&self) -> &Path {
            &self.root
        }
        fn function_registry(&self) -> Arc<FunctionRegistry> {
            Arc::default()
        }
        fn driver_schema(&self, _name: &str) -> Option<DriverSchema> {
            None
        }
        fn driver_names(&self) -> Vec<String> {
            Vec::new()
        }
        fn provider_names(&self) -> Vec<String> {
            Vec::new()
        }
        fn provider_state_schema(&self, _name: &str) -> Option<StateSchema> {
            None
        }
        fn provider_options(&self, _name: &str) -> Options {
            Options::default()
        }
        fn list(
            self: Arc<Self>,
            what: hplugin::lsp::Listing,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<hplugin::lsp::Listed>> {
            use futures::FutureExt as _;
            let fresh =
                super::list_with_fresh_buildfile_provider(self.root.clone(), Arc::default(), what);
            match self.list {
                ListMode::Fresh => fresh,
                ListMode::Fails => {
                    Box::pin(async { Err(anyhow::anyhow!("the provider exploded")) })
                }
                ListMode::Hangs => Box::pin(futures::future::pending()),
                ListMode::ScopeHeld => fresh
                    .map(|listed| {
                        listed.map(|mut l| {
                            l.scope_ended = Box::pin(futures::future::pending());
                            l
                        })
                    })
                    .boxed(),
            }
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// The typed prefix → what reaches the providers. Whatever would leave
    /// the workspace once a provider joins it to the root is refused before
    /// any provider sees it.
    #[test]
    fn packages_under_lists_only_paths_inside_the_workspace() {
        let under = |u: &str| {
            Ok(Listing::Packages {
                under: u.to_string(),
            })
        };
        let cases: [(&str, Result<Listing, BadPath>); 14] = [
            ("", under("")),
            ("fo", under("")),
            ("foo/", under("foo")),
            ("foo/ba", under("foo")),
            ("a/b/c/d", under("a/b/c")),
            ("@heph/go/g", under("@heph/go")),
            ("héllo/wörld", under("héllo")),
            // The partial segment only filters: `..` there names nothing.
            ("foo/..", under("foo")),
            ("a//b", Err(BadPath::EmptySegment)),
            ("../..", Err(BadPath::DotSegment)),
            ("./x", Err(BadPath::DotSegment)),
            ("/etc", Err(BadPath::Absolute)),
            ("/etc/x", Err(BadPath::Absolute)),
            ("a\0b/c", Err(BadPath::ControlByte)),
        ];
        for (prefix, want) in cases {
            assert_eq!(packages_under(prefix), want, "prefix {prefix:?}");
        }
        // A package is all whole segments.
        assert!(targets_in("").is_ok(), "the root package");
        assert!(targets_in("@heph/oci").is_ok());
        assert_eq!(targets_in("a/"), Err(BadPath::EmptySegment));
        assert_eq!(targets_in("a/../.."), Err(BadPath::DotSegment));
        assert_eq!(targets_in("/etc"), Err(BadPath::Absolute));
        assert_eq!(targets_in("a\tb"), Err(BadPath::ControlByte));
    }

    /// A provider that hangs costs a completion the listing wait, not the
    /// editor: the listing is dropped and the caller told it timed out.
    #[test]
    fn a_hung_listing_is_given_up_after_the_wait() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = runtime();
        let wait = Duration::from_millis(100);
        let ctx = rt.block_on(async {
            HephLspContext::new(
                Arc::new(FakeEngine {
                    root: dir.path().to_path_buf(),
                    list: ListMode::Hangs,
                }),
                wait,
            )
        });
        let started = Instant::now();
        let got = std::thread::spawn(move || {
            ctx.listing(Listing::Targets {
                package: String::new(),
            })
        })
        .join()
        .expect("no panic");
        assert!(
            matches!(got, Err(ListingError::TimedOut(w)) if w == wait),
            "{got:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// A listing that errs reaches the caller as `Failed`, with its cause.
    #[test]
    fn a_failed_listing_keeps_its_cause() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = runtime();
        let ctx = rt.block_on(async {
            HephLspContext::new(
                Arc::new(FakeEngine {
                    root: dir.path().to_path_buf(),
                    list: ListMode::Fails,
                }),
                LISTING_WAIT,
            )
        });
        let got = std::thread::spawn(move || {
            ctx.listing(Listing::Targets {
                package: String::new(),
            })
        })
        .join()
        .expect("no panic");
        assert!(
            matches!(&got, Err(ListingError::Failed(e)) if format!("{e:#}").contains("exploded")),
            "{got:?}"
        );
    }

    /// The LSP's listing calls must work from its own request thread, which has
    /// no ambient tokio runtime.
    ///
    /// This is the one production caller of `hcore::blocking::run` that is not
    /// already polled with a runtime context: `starlark_lsp` dispatches
    /// completion on a plain `std::thread`, and the package walk underneath
    /// reaches `blocking::run`, which needs a runtime to `spawn_blocking` on.
    /// `HephLspContext` captures the server's `Handle` at construction and
    /// enters it around each `block_on` for exactly this reason.
    ///
    /// Driven from a thread spawned outside the runtime, so the context's
    /// captured handle is the *only* runtime in reach. Remove either `enter()`
    /// and this panics with "there is no reactor running" (mutation-verified).
    #[test]
    fn listing_works_from_a_thread_with_no_ambient_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir_all(&pkg).expect("mkdir pkg");
        std::fs::write(pkg.join("BUILD"), "").expect("write BUILD");

        // Built inside a runtime (as the real server does — it captures
        // `Handle::current()`), then used from outside one.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let ctx = rt.block_on(async {
            HephLspContext::new(Arc::new(FakeEngine::new(dir.path())), LISTING_WAIT)
        });

        let listed = std::thread::spawn(move || {
            ctx.listing(Listing::Packages {
                under: String::new(),
            })
        })
        .join()
        .expect("the listing thread must not panic for want of a runtime")
        .expect("the listing answers");
        assert!(
            listed.entries.iter().any(|e| e.name == "mypkg"),
            "the package walk must reach the LSP caller: {listed:?}"
        );
    }

    /// Goto-definition must survive a buffer the engine refuses to evaluate.
    ///
    /// An unknown key on a macro call is an eval error, and the index built from
    /// a failed eval carries no provenance — but the `load(...)` imports that
    /// drive cross-file goto-definition are read from the parse, so they must
    /// still be there. Losing the jump exactly while the user is fixing the
    /// error is the worst moment to lose it.
    #[test]
    fn loaded_symbols_survive_a_buffer_that_fails_to_evaluate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let ctx = rt.block_on(async {
            HephLspContext::new(Arc::new(FakeEngine::new(dir.path())), LISTING_WAIT)
        });

        // `//lib` does not exist, so the load fails and the whole evaluation
        // with it — the same shape as any other error in the file.
        let content = "load(\"//lib\", \"make_name\")\n\ntarget(name = make_name(\"t\"))\n";
        let uri = super::LspUri::File(dir.path().join("BUILD"));
        // An open document: the loop stages its tokens before the parse.
        ctx.shared
            .stage_tokens(uri.clone(), Arc::new(super::super::lex::lex(content)));
        let result = ctx.parse_file_with_contents(&uri, content.to_string());
        assert!(
            result.ast.is_some(),
            "the buffer parses; only evaluation fails"
        );

        let index = ctx.shared.index(&uri).expect("index");
        assert!(
            index.call_targets.is_empty(),
            "a failed eval yields no provenance"
        );
        // The loaded names come from the parse, not the evaluation.
        let overlay = ctx.shared.overlay(&uri).expect("overlay");
        assert_eq!(
            overlay.loaded.get("make_name"),
            Some(&("//lib".to_string(), "make_name".to_string())),
            "the loaded symbol must still resolve for goto-definition"
        );
    }

    #[test]
    fn first_build_file_matches_literal_and_glob_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "").unwrap();
        std::fs::write(dir.path().join("pkg.BUILD"), "").unwrap();

        // Glob pattern resolves the `*.BUILD` file (not the .txt).
        let glob = vec![glob::Pattern::new("*.BUILD").unwrap()];
        let found = first_build_file(dir.path(), &glob).unwrap();
        assert_eq!(found.file_name().unwrap(), "pkg.BUILD");

        // A literal pattern that doesn't match anything → None.
        let literal = vec![glob::Pattern::new("BUILD").unwrap()];
        assert!(first_build_file(dir.path(), &literal).is_none());

        // Missing directory → None, not a panic.
        assert!(first_build_file(&dir.path().join("nope"), &glob).is_none());
    }
}
