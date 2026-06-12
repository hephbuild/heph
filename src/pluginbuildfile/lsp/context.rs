//! heph's [`LspContext`] implementation: it teaches `starlark_lsp` about heph's
//! globals (so the builtins complete and hover), resolves `load()` paths and
//! target-address string literals, completes target addresses, and — as a side
//! effect of evaluating each opened buffer — populates the per-document index the
//! proxy uses for the provenance hover and driver-schema completion.

use super::index::{DocIndex, SharedState};
use crate::engine::engine::Engine;
use crate::engine::provider::{
    ListPackagesRequest, ListRequest, Provider as EProvider, ProviderFunctionRegistry,
};
use crate::hasync::StdCancellationToken;
use crate::htpkg::PkgBuf;
use crate::pluginbuildfile::Provider;
use crate::pluginbuildfile::run_file::{BuildFileLoader, build_globals, eval_source};
use starlark::docs::DocModule;
use starlark::environment::Globals;
use starlark::errors::EvalMessage;
use starlark::syntax::{AstModule, Dialect};
use starlark_lsp::completion::{StringCompletionResult, StringCompletionType};
use starlark_lsp::error::eval_message_to_lsp_diagnostic;
use starlark_lsp::server::{LspContext, LspEvalResult, LspUrl, StringLiteralResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) struct HephLspContext {
    root: PathBuf,
    registry: Arc<ProviderFunctionRegistry>,
    /// Docs for the heph globals; drives `starlark_lsp`'s builtin completion/hover.
    doc_globals: DocModule,
    /// Lazily-built Starlark globals, shared across per-buffer evaluations.
    globals: Arc<OnceLock<Globals>>,
    /// BUILD-file name patterns, from the buildfile provider's `patterns` config
    /// option (defaulting to `BUILD`). Used for both listing and load resolution.
    patterns: Vec<glob::Pattern>,
    /// Buildfile provider used only for package/target listing during completion.
    provider: Provider,
    pub(crate) shared: Arc<SharedState>,
}

impl HephLspContext {
    pub(crate) fn new(engine: Arc<Engine>) -> HephLspContext {
        let root = engine.root().to_path_buf();
        let registry = engine.provider_function_registry();
        let doc_globals = build_globals(&registry).documentation();
        let patterns = buildfile_patterns(&root);
        // Listing provider configured with the same patterns the engine's
        // buildfile provider uses, so completion sees the right BUILD files.
        let provider = Provider {
            root: root.clone(),
            build_file_patterns: patterns.clone(),
            ..Provider::default()
        };
        provider.set_function_registry(Arc::clone(&registry));
        HephLspContext {
            root,
            registry,
            doc_globals,
            globals: Arc::new(OnceLock::new()),
            patterns,
            provider,
            shared: SharedState::new(engine),
        }
    }

    /// Package name for a BUILD file path (its parent dir, workspace-relative,
    /// forward-slashed). Empty string for a BUILD file at the workspace root.
    fn path_to_pkg(&self, path: &Path) -> String {
        let parent = path.parent().unwrap_or(path);
        let rel = parent.strip_prefix(&self.root).unwrap_or(parent);
        rel.to_string_lossy().replace('\\', "/")
    }

    fn loader(&self) -> BuildFileLoader {
        BuildFileLoader::new(
            self.root.clone(),
            self.patterns.clone(),
            // Fresh per-call caches: the open buffer may differ from disk, so the
            // main file must never be served from a stale cache.
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::clone(&self.registry),
            Arc::clone(&self.globals),
            Arc::new(crate::htwalk::CachedWalker::disabled()),
        )
    }

    /// List package names beginning with `prefix` (no leading `//`).
    fn list_packages(&self, prefix: &str) -> Vec<String> {
        let ctoken = StdCancellationToken::new();
        let fut = self.provider.list_packages(
            ListPackagesRequest {
                prefix: PkgBuf::from(prefix),
            },
            &ctoken,
        );
        match futures::executor::block_on(fut) {
            Ok(iter) => iter
                .filter_map(Result::ok)
                .map(|r| r.pkg.to_string())
                .filter(|p| p.starts_with(prefix))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// List target names in `package`.
    fn list_targets(&self, package: &str) -> Vec<String> {
        let ctoken = StdCancellationToken::new();
        let fut = self.provider.list(
            ListRequest {
                request_id: "lsp".to_string(),
                package: PkgBuf::from(package),
                states: vec![],
            },
            &ctoken,
        );
        match futures::executor::block_on(fut) {
            Ok(iter) => iter
                .filter_map(Result::ok)
                .map(|r| r.addr.name.clone())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The on-disk BUILD file URL for a package: the first file in the package
    /// dir whose name matches a configured pattern (handles literal names like
    /// `BUILD` and globs like `*.BUILD` alike).
    fn build_file_url(&self, package: &str) -> Option<LspUrl> {
        let dir = self.root.join(package);
        let entry = std::fs::read_dir(&dir).ok()?.flatten().find(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && self.patterns.iter().any(|p| p.matches(&name))
        })?;
        lsp_types::Url::from_file_path(entry.path())
            .ok()
            .and_then(|u| LspUrl::try_from(u).ok())
    }
}

/// BUILD-file name patterns from the workspace's buildfile-provider config,
/// mirroring what the engine's provider uses. Falls back to `["BUILD"]` when the
/// config is absent, lists no patterns, or none compile.
fn buildfile_patterns(root: &Path) -> Vec<glob::Pattern> {
    use crate::engine::config_yaml;
    let names: Vec<String> = config_yaml::load_from_root(root)
        .ok()
        .and_then(|cfg| {
            cfg.providers
                .iter()
                .find(|p| p.name == "buildfile")
                .and_then(|p| {
                    config_yaml::decode_opt::<Vec<String>>(
                        &p.options,
                        "buildfile provider",
                        "patterns",
                    )
                    .ok()
                    .flatten()
                })
        })
        .unwrap_or_default();
    let compiled: Vec<glob::Pattern> = names
        .iter()
        .filter_map(|n| glob::Pattern::new(n).ok())
        .collect();
    if compiled.is_empty() {
        vec![glob::Pattern::new("BUILD").expect("BUILD literal")]
    } else {
        compiled
    }
}

impl LspContext for HephLspContext {
    fn parse_file_with_contents(&self, uri: &LspUrl, content: String) -> LspEvalResult {
        let path = match uri {
            LspUrl::File(p) | LspUrl::Starlark(p) => p.clone(),
            _ => return LspEvalResult::default(),
        };
        let filename = path.to_string_lossy().to_string();

        // Parse the AST for starlark_lsp (symbols, hover, goto); parse errors
        // become diagnostics.
        let ast = match AstModule::parse(&filename, content.clone(), &Dialect::Extended) {
            Ok(ast) => ast,
            Err(e) => {
                return LspEvalResult {
                    diagnostics: vec![eval_message_to_lsp_diagnostic(EvalMessage::from_error(
                        &path, &e,
                    ))],
                    ast: None,
                };
            }
        };

        // Best-effort evaluation to populate the provenance / driver index. A
        // half-typed buffer that fails to evaluate simply yields no index — we do
        // not surface eval errors as diagnostics (they would be noisy while typing).
        let pkg = self.path_to_pkg(&path);
        if let Ok(result) = eval_source(&filename, content, &pkg, &self.loader()) {
            self.shared
                .set_index(uri.clone(), DocIndex::build(&result, &pkg));
        } else {
            self.shared.set_index(uri.clone(), DocIndex::default());
        }

        LspEvalResult {
            diagnostics: vec![],
            ast: Some(ast),
        }
    }

    fn resolve_load(
        &self,
        path: &str,
        current_file: &LspUrl,
        _workspace_root: Option<&Path>,
    ) -> anyhow::Result<LspUrl> {
        // `//pkg:file` → <root>/pkg/file ; `:file` or relative → next to current.
        let resolved = if let Some(rest) = path.strip_prefix("//") {
            let (pkg, file) = rest.split_once(':').unwrap_or((rest, ""));
            self.root.join(pkg).join(file)
        } else {
            let rel = path.strip_prefix(':').unwrap_or(path);
            match current_file {
                LspUrl::File(p) => p.parent().unwrap_or(Path::new("")).join(rel),
                _ => PathBuf::from(rel),
            }
        };
        Ok(LspUrl::try_from(
            lsp_types::Url::from_file_path(&resolved)
                .map_err(|()| anyhow::anyhow!("non-file load path"))?,
        )?)
    }

    fn render_as_load(
        &self,
        target: &LspUrl,
        _current_file: &LspUrl,
        _workspace_root: Option<&Path>,
    ) -> anyhow::Result<String> {
        match target {
            LspUrl::File(target_path) => {
                let pkg = self.path_to_pkg(target_path);
                let file = target_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                Ok(format!("//{pkg}:{file}"))
            }
            _ => anyhow::bail!("can only render file:// load paths"),
        }
    }

    fn resolve_string_literal(
        &self,
        literal: &str,
        current_file: &LspUrl,
        _workspace_root: Option<&Path>,
    ) -> anyhow::Result<Option<StringLiteralResult>> {
        // Goto-definition on a target address: `//pkg:name` (absolute) or `:name`
        // (relative to the current file's package). Jump to the package's BUILD
        // file. Non-address strings resolve to nothing.
        let package = if let Some(rest) = literal.strip_prefix("//") {
            rest.split_once(':').map(|(p, _)| p.to_string())
        } else if let Some(_name) = literal.strip_prefix(':') {
            match current_file {
                LspUrl::File(p) => Some(self.path_to_pkg(p)),
                _ => None,
            }
        } else {
            None
        };

        Ok(package.and_then(|pkg| {
            self.build_file_url(&pkg).map(|url| StringLiteralResult {
                url,
                location_finder: None,
            })
        }))
    }

    fn get_load_contents(&self, uri: &LspUrl) -> anyhow::Result<Option<String>> {
        match uri {
            LspUrl::File(p) => Ok(std::fs::read_to_string(p).ok()),
            _ => Ok(None),
        }
    }

    fn get_environment(&self, _uri: &LspUrl) -> DocModule {
        self.doc_globals.clone()
    }

    fn get_url_for_global_symbol(
        &self,
        _current_file: &LspUrl,
        _symbol: &str,
    ) -> anyhow::Result<Option<LspUrl>> {
        Ok(None)
    }

    fn get_string_completion_options(
        &self,
        _document_uri: &LspUrl,
        kind: StringCompletionType,
        current_value: &str,
        _workspace_root: Option<&Path>,
    ) -> anyhow::Result<Vec<StringCompletionResult>> {
        // Only complete target-address-looking strings (skip load-path strings,
        // which starlark_lsp handles via resolve_load).
        if !matches!(kind, StringCompletionType::String) {
            return Ok(vec![]);
        }
        let Some(rest) = current_value.strip_prefix("//") else {
            return Ok(vec![]);
        };

        let results = match rest.split_once(':') {
            // After the colon: complete target names in the (typed) package.
            Some((pkg, name_prefix)) => self
                .list_targets(pkg)
                .into_iter()
                .filter(|n| n.starts_with(name_prefix))
                .map(|name| StringCompletionResult {
                    value: format!("//{pkg}:{name}"),
                    insert_text: None,
                    insert_text_offset: 0,
                    kind: lsp_types::CompletionItemKind::VALUE,
                })
                .collect(),
            // Before the colon: complete package names.
            None => self
                .list_packages(rest)
                .into_iter()
                .map(|pkg| StringCompletionResult {
                    value: format!("//{pkg}"),
                    insert_text: None,
                    insert_text_offset: 0,
                    kind: lsp_types::CompletionItemKind::MODULE,
                })
                .collect(),
        };
        Ok(results)
    }
}
