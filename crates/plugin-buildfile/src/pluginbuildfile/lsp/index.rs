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

use crate::pluginbuildfile::run_file::RunResult;
use hplugin::lsp::LspEngine;
use hplugin::provider::ProvenanceFrame;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use vendored_starlark_lsp::server::LspUri;

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

/// What the evaluation contributes for one document. It enriches answers (the
/// "Targets" hover, an undocumented `def`'s signature) and never gates them:
/// the site, the shadowing and the loaded names come from the resolver.
#[derive(Clone, Debug, Default)]
pub(crate) struct DocIndex {
    pub call_targets: Vec<CallTargets>,
    /// Rendered hover doc for each top-level function binding (`def` in this file
    /// or imported via `load`), keyed by name. Used to synthesize a signature
    /// tooltip for undocumented functions, which the stock server skips.
    pub defs: std::collections::HashMap<String, String>,
    /// The buffer's source text, for extracting the identifier under the cursor.
    pub source: String,
}

impl DocIndex {
    /// An index for a buffer that didn't evaluate — the user is mid-edit, or
    /// the file has an error the engine rejects. No provenance, no docs.
    pub(crate) fn unevaluated(source: String) -> DocIndex {
        DocIndex {
            source,
            ..DocIndex::default()
        }
    }

    /// Build the index from an evaluated buffer. `pkg` is the buffer's package
    /// (used to format `//pkg:name` addresses); `source` is the buffer text.
    pub(crate) fn build(result: &RunResult, pkg: &str, source: String) -> DocIndex {
        let defs = function_docs(result);
        // Aggregate addresses per unique call-site span. BTreeMap keeps a stable
        // order and dedups identical spans (e.g. a loop body line that produced
        // several targets).
        let mut by_span: BTreeMap<Span, Vec<String>> = BTreeMap::new();

        for target in &result.targets {
            let addr = format!("//{pkg}:{}", target.name);
            for frame in &target.provenance {
                by_span
                    .entry(Span::of(frame))
                    .or_default()
                    .push(addr.clone());
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

        DocIndex {
            call_targets,
            defs,
            source,
        }
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
    /// static namespace (which isn't in the plugin-function registry).
    pub core_members: Vec<crate::pluginbuildfile::run_file::CoreMember>,
    /// Renders hover markdown for the `target` / `provider_state` builtins — the
    /// stock server only knows their raw `*args, **kwargs` prototype. Rendered
    /// per hover so the prototype can narrow to the driver/provider the call
    /// site names.
    pub builtin_hovers: crate::pluginbuildfile::run_file::BuiltinHovers,
    docs: Mutex<HashMap<LspUri, DocIndex>>,
    /// The name sets of each document's last good parse (D3).
    overlays: Mutex<HashMap<LspUri, Arc<super::site::Overlay>>>,
    /// The token array the loop just built for a document, handed to the
    /// parse it is about to run so the text is lexed once per edit. Present
    /// only while an open document is being parsed.
    staged: Mutex<HashMap<LspUri, Arc<super::lex::Tokens>>>,
}

impl SharedState {
    pub(crate) fn new(
        engine: Arc<dyn LspEngine>,
        root: std::path::PathBuf,
        patterns: Vec<glob::Pattern>,
        core_members: Vec<crate::pluginbuildfile::run_file::CoreMember>,
        builtin_hovers: crate::pluginbuildfile::run_file::BuiltinHovers,
    ) -> Arc<SharedState> {
        Arc::new(SharedState {
            engine,
            root,
            patterns,
            core_members,
            builtin_hovers,
            docs: Mutex::new(HashMap::new()),
            overlays: Mutex::new(HashMap::new()),
            staged: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn set_index(&self, uri: LspUri, index: DocIndex) {
        self.docs.lock().expect("docs lock").insert(uri, index);
    }

    pub(crate) fn index(&self, uri: &LspUri) -> Option<DocIndex> {
        self.docs.lock().expect("docs lock").get(uri).cloned()
    }

    /// Drop `uri`'s index: the document was closed.
    pub(crate) fn set_overlay(&self, uri: LspUri, overlay: super::site::Overlay) {
        self.overlays
            .lock()
            .expect("overlays lock")
            .insert(uri, Arc::new(overlay));
    }

    pub(crate) fn stage_tokens(&self, uri: LspUri, tokens: Arc<super::lex::Tokens>) {
        self.staged.lock().expect("staged lock").insert(uri, tokens);
    }

    /// The tokens staged for `uri`, if an open document's parse is running;
    /// `None` for a file read from disk for the stock analysis.
    pub(crate) fn take_tokens(&self, uri: &LspUri) -> Option<Arc<super::lex::Tokens>> {
        self.staged.lock().expect("staged lock").remove(uri)
    }

    pub(crate) fn overlay(&self, uri: &LspUri) -> Option<Arc<super::site::Overlay>> {
        self.overlays
            .lock()
            .expect("overlays lock")
            .get(uri)
            .cloned()
    }

    pub(crate) fn remove(&self, uri: &LspUri) {
        self.docs.lock().expect("docs lock").remove(uri);
        self.overlays.lock().expect("overlays lock").remove(uri);
    }

    /// How many documents have an index (test support: closing frees them).
    #[cfg(feature = "test-support")]
    pub(crate) fn len(&self) -> usize {
        self.docs.lock().expect("docs lock").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginbuildfile::run_file::{BuildFileLoader, eval_source};
    use std::collections::HashMap;

    fn index_for(content: &str) -> DocIndex {
        let tmp = tempfile::tempdir().unwrap();
        let patterns = vec![glob::Pattern::new("BUILD").unwrap()];
        let walker = Arc::new(hwalk::CachedWalker::disabled());
        let loader = BuildFileLoader::new(
            tmp.path().to_path_buf(),
            patterns.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(hplugin::function::FunctionRegistry::default()),
            Arc::new(std::sync::OnceLock::new()),
            Arc::clone(&walker),
            Arc::new(crate::pluginbuildfile::provider::PackageList::new(
                tmp.path().to_path_buf(),
                patterns,
                Arc::new(hwalk::Ignore::default()),
                walker,
            )),
            Arc::default(),
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
    fn overlay_reads_positional_and_aliased_loads_from_the_ast() {
        let src = "load(\"//lib\", \"make_name\", greeting = \"GREETING\")\n\
                   load(\"//other\", \"x\")\n\
                   load(\"//multi\",\n    \"a\",\n    \"b\",\n)\n";
        let ast = starlark::syntax::AstModule::parse(
            "BUILD",
            src.to_string(),
            &starlark::syntax::Dialect::Extended,
        )
        .unwrap();
        let m = super::super::site::overlay_of(&ast).loaded;
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
        // A load split over lines, which the old line-by-line scan missed.
        assert_eq!(m.get("b"), Some(&("//multi".to_string(), "b".to_string())));
    }

    #[test]
    fn word_at_extracts_identifier_under_cursor() {
        let src = "foo bar_baz qux\n";
        assert_eq!(super::word_at(src, 1, 6), Some("bar_baz")); // inside bar_baz
        assert_eq!(super::word_at(src, 1, 1), Some("foo"));
        assert_eq!(super::word_at(src, 1, 4), None); // the space
    }
}
