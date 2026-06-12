//! Per-document index the LSP builds while evaluating a BUILD buffer, and the
//! shared state the proxy reads to enrich hover/completion responses.
//!
//! Two things are indexed from a successful evaluation:
//! - **call → targets**: every source call site (the `target()` call and the
//!   macros/loops above it) mapped to the addresses it produced. Drives the
//!   "Generated targets" hover.
//! - **target-call → driver**: each `target()` call's source span and its
//!   `driver`, so completion inside that call can offer the driver's config
//!   fields.

use crate::engine::engine::Engine;
use crate::engine::provider::ProvenanceFrame;
use crate::pluginbuildfile::run_file::RunResult;
use starlark_lsp::server::LspUrl;
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

/// Everything the proxy needs to answer position-based requests for one document.
#[derive(Clone, Debug, Default)]
pub(crate) struct DocIndex {
    pub call_targets: Vec<CallTargets>,
    pub target_calls: Vec<TargetCall>,
}

impl DocIndex {
    /// Build the index from an evaluated buffer. `pkg` is the buffer's package
    /// (used to format `//pkg:name` addresses).
    pub(crate) fn build(result: &RunResult, pkg: &str) -> DocIndex {
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
            // The innermost frame is the `target()` call itself.
            if let Some(inner) = target.provenance.first()
                && !target.driver.is_empty()
            {
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

        DocIndex {
            call_targets,
            target_calls,
        }
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

    /// The driver of the `target()` call covering the 1-based position, if any.
    pub(crate) fn driver_at(&self, line: u32, col: u32) -> Option<&str> {
        self.target_calls
            .iter()
            .find(|tc| tc.span.covers(line, col))
            .map(|tc| tc.driver.as_str())
    }
}

/// State shared between the [`super::context::HephLspContext`] (writer) and the
/// [`super::proxy`] (reader): the per-document indexes plus the engine handle the
/// proxy uses to resolve driver schemas.
pub(crate) struct SharedState {
    pub engine: Arc<Engine>,
    docs: Mutex<HashMap<LspUrl, DocIndex>>,
}

impl SharedState {
    pub(crate) fn new(engine: Arc<Engine>) -> Arc<SharedState> {
        Arc::new(SharedState {
            engine,
            docs: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn set_index(&self, uri: LspUrl, index: DocIndex) {
        self.docs.lock().expect("docs lock").insert(uri, index);
    }

    pub(crate) fn index(&self, uri: &LspUrl) -> Option<DocIndex> {
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
            Arc::new(crate::engine::provider::ProviderFunctionRegistry::default()),
            Arc::new(std::sync::OnceLock::new()),
            Arc::new(crate::htwalk::CachedWalker::disabled()),
        );
        let result = eval_source("BUILD", content.to_string(), "pkg", &loader).unwrap();
        DocIndex::build(&result, "pkg")
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
}
