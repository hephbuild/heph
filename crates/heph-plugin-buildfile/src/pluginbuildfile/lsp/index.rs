//! Per-document index the LSP builds while evaluating a BUILD buffer, and the
//! shared state the proxy reads to enrich hover/completion responses.
//!
//! Two things are indexed from a successful evaluation:
//! - **call → targets**: every source call site (the `target()` call and the
//!   macros/loops above it) mapped to the addresses it produced. Drives the
//!   "Targets" hover.
//! - **target-call → driver**: each `target()` call's source span and its
//!   `driver`, so completion inside that call can offer the driver's config
//!   fields.

use heph_plugin::lsp::LspEngine;
use heph_plugin::provider::ProvenanceFrame;
use crate::pluginbuildfile::run_file::RunResult;
use starlark_lsp::server::LspUri;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

/// A 1-based source span within a single file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Span {
    pub line_start: u32,
    pub col_start: u32,
    pub line_end: u32,
    pub col_end: u32,
}

impl Span {
    fn of(frame: &ProvenanceFrame) -> Span {
        Span {
            line_start: frame.line_start,
            col_start: frame.col_start,
            line_end: frame.line_end,
            col_end: frame.col_end,
        }
    }

    /// Whether this span covers the 1-based (line, col) position.
    fn covers(&self, line: u32, col: u32) -> bool {
        (line, col) >= (self.line_start, self.col_start)
            && (line, col) <= (self.line_end, self.col_end)
    }
}

/// A call site mapped to the target addresses that originated from it.
#[derive(Clone, Debug)]
pub(crate) struct CallTargets {
    pub span: Span,
    pub addrs: Vec<String>,
}

/// A `target()` call site and the driver it selected.
#[derive(Clone, Debug)]
pub(crate) struct TargetCall {
    pub span: Span,
    pub driver: String,
}

/// A `provider_state()` call site and the provider it targets.
#[derive(Clone, Debug)]
pub(crate) struct StateCall {
    pub span: Span,
    pub provider: String,
}

/// Everything the proxy needs to answer position-based requests for one document.
#[derive(Clone, Debug, Default)]
pub(crate) struct DocIndex {
    pub call_targets: Vec<CallTargets>,
    pub target_calls: Vec<TargetCall>,
    pub state_calls: Vec<StateCall>,
    /// Rendered hover doc for each top-level function binding (`def` in this file
    /// or imported via `load`), keyed by name. Used to synthesize a signature
    /// tooltip for undocumented functions, which the stock server skips.
    pub defs: std::collections::HashMap<String, String>,
    /// Symbols imported via `load`, keyed by their local name → (load path,
    /// exported name in the loaded package). Drives cross-file goto-definition:
    /// heph merges every BUILD file in a package, but the stock server only parses
    /// one, so we resolve loaded symbols across all of them ourselves.
    pub loaded: HashMap<String, (String, String)>,
    /// The buffer's source text, for extracting the identifier under the cursor.
    pub source: String,
}

impl DocIndex {
    /// An index carrying only the buffer text, no provenance. Used when the
    /// buffer doesn't parse/evaluate (the user is mid-edit, e.g. just typed
    /// `heph.`) so prefix-based completion/hover still has the current source.
    pub(crate) fn source_only(source: String) -> DocIndex {
        DocIndex {
            source,
            ..DocIndex::default()
        }
    }

    /// Build the index from an evaluated buffer. `pkg` is the buffer's package
    /// (used to format `//pkg:name` addresses); `source` is the buffer text.
    pub(crate) fn build(result: &RunResult, pkg: &str, source: String) -> DocIndex {
        let defs = function_docs(result);
        let loaded = parse_load_symbols(&source);
        // Aggregate addresses per unique call-site span. BTreeMap keeps a stable
        // order and dedups identical spans (e.g. a loop body line that produced
        // several targets).
        let mut by_span: BTreeMap<Span, Vec<String>> = BTreeMap::new();
        let mut target_calls = Vec::new();

        for target in &result.targets {
            let addr = format!("//{pkg}:{}", target.name);
            for frame in &target.provenance {
                by_span
                    .entry(Span::of(frame))
                    .or_default()
                    .push(addr.clone());
            }
            // The innermost frame is the `target()` call itself. Recorded for every
            // target (driver may be empty) so completion can offer the base
            // `target()` args even before a driver is chosen.
            if let Some(inner) = target.provenance.first() {
                target_calls.push(TargetCall {
                    span: Span::of(inner),
                    driver: target.driver.clone(),
                });
            }
        }

        let call_targets = by_span
            .into_iter()
            .map(|(span, mut addrs)| {
                addrs.sort();
                addrs.dedup();
                CallTargets { span, addrs }
            })
            .collect();

        // `provider_state()` call sites → provider, for state-arg completion.
        let state_calls = result
            .states
            .iter()
            .filter_map(|s| {
                let inner = s.provenance.first()?;
                Some(StateCall {
                    span: Span::of(inner),
                    provider: s.provider.clone(),
                })
            })
            .collect();

        DocIndex {
            call_targets,
            target_calls,
            state_calls,
            defs,
            loaded,
            source,
        }
    }

    /// The provider of the `provider_state()` call covering the 1-based position.
    pub(crate) fn state_provider_at(&self, line: u32, col: u32) -> Option<&str> {
        self.state_calls
            .iter()
            .find(|sc| sc.span.covers(line, col))
            .map(|sc| sc.provider.as_str())
    }

    /// The load path and exported name for the loaded symbol at the 1-based
    /// position, if the identifier under the cursor was imported via `load`.
    pub(crate) fn loaded_symbol_at(&self, line: u32, col: u32) -> Option<(&str, &str)> {
        let word = word_at(&self.source, line, col)?;
        self.loaded
            .get(word)
            .map(|(path, exported)| (path.as_str(), exported.as_str()))
    }

    /// Rendered hover for the function named by the identifier at the 1-based
    /// position, if that identifier resolves to a known `def`.
    pub(crate) fn def_hover_at(&self, line: u32, col: u32) -> Option<&str> {
        let word = word_at(&self.source, line, col)?;
        self.defs.get(word).map(String::as_str)
    }

    /// The most specific (smallest) call site covering the 1-based position and
    /// the addresses it produced.
    pub(crate) fn targets_at(&self, line: u32, col: u32) -> Option<&[String]> {
        self.call_targets
            .iter()
            .filter(|ct| ct.span.covers(line, col))
            // Prefer the innermost (smallest) covering span.
            .min_by_key(|ct| {
                (
                    ct.span.line_end - ct.span.line_start,
                    ct.span.col_end.saturating_sub(ct.span.col_start),
                )
            })
            .map(|ct| ct.addrs.as_slice())
    }

    /// If the 1-based position is inside a `target()` call, its driver (which may
    /// be the empty string when no `driver=` is set yet); `None` if not in a
    /// `target()` call.
    pub(crate) fn driver_at(&self, line: u32, col: u32) -> Option<&str> {
        self.target_calls
            .iter()
            .find(|tc| tc.span.covers(line, col))
            .map(|tc| tc.driver.as_str())
    }
}

/// Render a hover doc for every top-level function binding in the evaluated
/// module — `def`s in this file and functions pulled in via `load`. Non-function
/// bindings (constants, target addresses) are skipped.
fn function_docs(result: &RunResult) -> HashMap<String, String> {
    use starlark::docs::{DocItem, DocMember};
    let mut out = HashMap::new();
    for name in result.module.names() {
        let name = name.as_str();
        let Ok(value) = result.module.get(name) else {
            continue;
        };
        let doc = value.value().documentation();
        if matches!(doc, DocItem::Member(DocMember::Function(_))) {
            out.insert(
                name.to_string(),
                starlark::docs::markdown::render_doc_item_no_link(name, &doc),
            );
        }
    }
    out
}

/// Parse `load(...)` statements out of the source, mapping each imported local
/// name to `(load path, exported name)`. Handles positional imports
/// (`load("//p", "sym")` → local == exported == `sym`) and aliased ones
/// (`load("//p", alias = "sym")` → local `alias`, exported `sym`). A best-effort
/// textual scan — load statements are simple, single-line in practice.
fn parse_load_symbols(source: &str) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("load(") else {
            continue;
        };
        let Some(close) = rest.find(')') else {
            continue;
        };
        let Some(args) = rest.get(..close) else {
            continue;
        };
        // Split on commas (symbol/path strings never contain commas).
        let mut parts = args.split(',');
        let Some(path) = parts.next().and_then(unquote) else {
            continue;
        };
        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match part.split_once('=') {
                // `alias = "exported"`
                Some((local, exported)) => {
                    let local = local.trim();
                    if let Some(exported) = unquote(exported)
                        && !local.is_empty()
                    {
                        out.insert(local.to_string(), (path.clone(), exported));
                    }
                }
                // positional `"sym"` — local name equals exported name
                None => {
                    if let Some(sym) = unquote(part) {
                        out.insert(sym.clone(), (path.clone(), sym));
                    }
                }
            }
        }
    }
    out
}

/// Strip surrounding single or double quotes from a trimmed token.
fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes.first() == Some(&b'"') || bytes.first() == Some(&b'\''))
        && bytes.last() == bytes.first()
    {
        s.get(1..s.len() - 1).map(str::to_string)
    } else {
        None
    }
}

/// The identifier (ASCII alphanumeric + `_`) under the 1-based `(line, col)` in
/// `source`. Returns `None` when the cursor isn't on an identifier character —
/// Starlark identifiers are ASCII, so byte indexing is safe here.
fn word_at(source: &str, line: u32, col: u32) -> Option<&str> {
    let line_str = source.lines().nth((line.checked_sub(1)?) as usize)?;
    let col0 = col.checked_sub(1)? as usize;
    let b = line_str.as_bytes();
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    if !b.get(col0).copied().is_some_and(is_ident) {
        return None;
    }
    let mut start = col0;
    while start > 0 && b.get(start - 1).copied().is_some_and(is_ident) {
        start -= 1;
    }
    let mut end = col0 + 1;
    while b.get(end).copied().is_some_and(is_ident) {
        end += 1;
    }
    line_str.get(start..end)
}

/// State shared between the [`super::context::HephLspContext`] (writer) and the
/// [`super::proxy`] (reader): the per-document indexes plus the engine handle the
/// proxy uses to resolve driver schemas.
pub(crate) struct SharedState {
    pub engine: Arc<dyn LspEngine>,
    /// Workspace root + BUILD-file patterns, so the proxy can resolve a loaded
    /// symbol's package directory and scan its files for the definition.
    pub root: std::path::PathBuf,
    pub patterns: Vec<glob::Pattern>,
    /// The `heph.core` builtin functions, for member completion/hover of that
    /// static namespace (which isn't in the provider-function registry).
    pub core_members: Vec<crate::pluginbuildfile::run_file::CoreMember>,
    docs: Mutex<HashMap<LspUri, DocIndex>>,
}

impl SharedState {
    pub(crate) fn new(
        engine: Arc<dyn LspEngine>,
        root: std::path::PathBuf,
        patterns: Vec<glob::Pattern>,
        core_members: Vec<crate::pluginbuildfile::run_file::CoreMember>,
    ) -> Arc<SharedState> {
        Arc::new(SharedState {
            engine,
            root,
            patterns,
            core_members,
            docs: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn set_index(&self, uri: LspUri, index: DocIndex) {
        self.docs.lock().expect("docs lock").insert(uri, index);
    }

    pub(crate) fn index(&self, uri: &LspUri) -> Option<DocIndex> {
        self.docs.lock().expect("docs lock").get(uri).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginbuildfile::run_file::{BuildFileLoader, eval_source};
    use std::collections::HashMap;

    fn index_for(content: &str) -> DocIndex {
        let tmp = tempfile::tempdir().unwrap();
        let loader = BuildFileLoader::new(
            tmp.path().to_path_buf(),
            vec![glob::Pattern::new("BUILD").unwrap()],
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(heph_plugin::provider::ProviderFunctionRegistry::default()),
            Arc::new(std::sync::OnceLock::new()),
            Arc::new(heph_walk::CachedWalker::disabled()),
        );
        let result = eval_source("BUILD", content.to_string(), "pkg", &loader).unwrap();
        DocIndex::build(&result, "pkg", content.to_string())
    }

    #[test]
    fn hover_over_macro_call_lists_all_generated_targets() {
        // Line 6 is the `my_macro("m")` call; line 7 is the direct target().
        let content = "\
def my_macro(prefix):
    target(name = prefix + \"_a\", driver = \"exec\")
    target(name = prefix + \"_b\", driver = \"exec\")

my_macro(\"m\")
target(name = \"direct\", driver = \"exec\")
";
        let index = index_for(content);

        // Hovering the macro call site (line 5, 1-based) lists both generated targets.
        let at_macro = index.targets_at(5, 1).unwrap();
        assert_eq!(
            at_macro,
            &["//pkg:m_a".to_string(), "//pkg:m_b".to_string()]
        );

        // Hovering the direct target() (line 6) lists only it.
        let at_direct = index.targets_at(6, 1).unwrap();
        assert_eq!(at_direct, &["//pkg:direct".to_string()]);

        // A blank position yields nothing.
        assert!(index.targets_at(4, 1).is_none());
    }

    #[test]
    fn driver_at_target_call_resolves_driver() {
        let content = "target(name = \"t\", driver = \"exec\")\n";
        let index = index_for(content);
        // The target() call is on line 1.
        assert_eq!(index.driver_at(1, 1), Some("exec"));
        assert!(index.driver_at(50, 1).is_none());
    }

    #[test]
    fn state_provider_at_resolves_provider() {
        let content = "provider_state(provider = \"go\", go_codegen_root = True)\n";
        let index = index_for(content);
        // Cursor inside the provider_state() call on line 1.
        assert_eq!(index.state_provider_at(1, 1), Some("go"));
        assert!(index.state_provider_at(50, 1).is_none());
    }

    #[test]
    fn def_hover_renders_signature_for_undocumented_function() {
        // No docstring — the stock server gives no hover; we synthesize one.
        let content = "def my_fn(a, b):\n    return a\n\nmy_fn(1, 2)\n";
        let index = index_for(content);
        // Hover the usage `my_fn(1, 2)` on line 4.
        let doc = index.def_hover_at(4, 1).expect("def hover");
        assert!(
            doc.contains("my_fn"),
            "hover should name the function: {doc}"
        );
        assert!(doc.contains('a') && doc.contains('b'), "params: {doc}");
        // A non-identifier / unknown word yields nothing.
        assert!(index.def_hover_at(2, 1).is_none() || !index.defs.contains_key("return"));
    }

    #[test]
    fn parse_load_symbols_handles_positional_and_aliased() {
        let src = r#"load("//lib", "make_name", greeting = "GREETING")
load("//other", "x")
"#;
        let m = super::parse_load_symbols(src);
        // positional: local == exported
        assert_eq!(
            m.get("make_name"),
            Some(&("//lib".to_string(), "make_name".to_string()))
        );
        // aliased: local `greeting` → exported `GREETING`
        assert_eq!(
            m.get("greeting"),
            Some(&("//lib".to_string(), "GREETING".to_string()))
        );
        assert_eq!(m.get("x"), Some(&("//other".to_string(), "x".to_string())));
    }

    #[test]
    fn word_at_extracts_identifier_under_cursor() {
        let src = "foo bar_baz qux\n";
        assert_eq!(super::word_at(src, 1, 6), Some("bar_baz")); // inside bar_baz
        assert_eq!(super::word_at(src, 1, 1), Some("foo"));
        assert_eq!(super::word_at(src, 1, 4), None); // the space
    }
}
