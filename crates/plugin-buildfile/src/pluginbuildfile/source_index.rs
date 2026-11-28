//! The provider's [`SourceIndex`]: a BUILD or `.bzl` file parsed again,
//! never evaluated, on a diagnostic's error path, to find one keyword
//! argument of the `target()` call a spec's [`SpecSource`] names (D3).

use crate::pluginbuildfile::ast::walk_expr;
use hcore::diagnostic::Span;
use hplugin::provider::{SourceIndex, SpecSource};
use starlark::syntax::AstModule;
use starlark::syntax::ast::{Argument, AstLiteral, Expr};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// `file` (workspace-relative, under `root`) parsed as it is on disk now, or
/// `None` when it is gone or no longer parses: every diagnostic then points
/// at its call.
pub(crate) fn index(root: &Path, file: &str) -> Option<Arc<dyn SourceIndex>> {
    let ast = crate::pluginbuildfile::run_file::parse_build_file(&root.join(file))
        .inspect_err(|e| tracing::debug!(file, error = %e, "re-parsing for a diagnostic"))
        .ok()?;
    Some(Arc::new(Parsed::new(ast)))
}

/// A parsed file and its calls by where each starts: one walk of the file
/// when the index is built, so each [`locate`](SourceIndex::locate) is a
/// lookup however many calls the file holds.
struct Parsed {
    ast: AstModule,
    calls: HashMap<(u32, u32), Call>,
}

/// A call [`Parsed`] found.
enum Call {
    /// A `target(...)` call: its `name=` when that is a string literal (a
    /// macro's `name = name` is not), and its keyword arguments, each with
    /// the span of `key = value`.
    Target {
        name: Option<String>,
        kwargs: Vec<(String, starlark::codemap::Span)>,
    },
    /// Any other callee (a plugin function that declared the target, say),
    /// whose arguments are not the spec's options.
    Other,
}

impl Parsed {
    fn new(ast: AstModule) -> Self {
        let mut calls = HashMap::new();
        ast.statement().visit_expr(|top| {
            walk_expr(top, &mut |e| {
                let Expr::Call(callee, args) = &e.node else {
                    return;
                };
                #[cfg(test)]
                tests::CALLS_VISITED.with(|n| n.set(n.get() + 1));
                let span = to_span(&ast, e.span);
                let is_target =
                    matches!(&callee.node, Expr::Identifier(id) if id.node.ident == "target");
                let call = if is_target {
                    let mut name = None;
                    let mut kwargs = Vec::new();
                    for arg in &args.args {
                        let Argument::Named(key, value) = &arg.node else {
                            continue;
                        };
                        if key.node == "name"
                            && let Expr::Literal(AstLiteral::String(s)) = &value.node
                        {
                            name = Some(s.node.clone());
                        }
                        kwargs.push((key.node.clone(), key.span.merge(value.span)));
                    }
                    Call::Target { name, kwargs }
                } else {
                    Call::Other
                };
                // Two calls start at one place only when one is the other's
                // callee (`f(x)(y)`): a `target` call wins.
                let slot = calls
                    .entry((span.start_line, span.start_col))
                    .or_insert(Call::Other);
                if matches!(slot, Call::Other) {
                    *slot = call;
                }
            });
        });
        Self { ast, calls }
    }
}

/// A codemap span as heph's: 1-based lines and columns.
fn to_span(ast: &AstModule, span: starlark::codemap::Span) -> Span {
    let rs = ast.file_span(span).resolve_span();
    let one_based = |n: usize| u32::try_from(n).unwrap_or(u32::MAX - 1).saturating_add(1);
    Span {
        start_line: one_based(rs.begin.line),
        start_col: one_based(rs.begin.column),
        end_line: one_based(rs.end.line),
        end_col: one_based(rs.end.column),
    }
}

impl SourceIndex for Parsed {
    fn locate(&self, source: &SpecSource, name: &str, field: &str) -> Option<Span> {
        let at = source.call();
        let Call::Target { name: literal, kwargs } =
            self.calls.get(&(at.start_line, at.start_col))?
        else {
            return None;
        };
        // The call that was there at eval, or the file changed since: a
        // literal `name=` must still be the target's. A computed one (a
        // macro's `name = name`) is not checked.
        if literal.as_deref().is_some_and(|n| n != name) {
            return None;
        }
        let (_, span) = kwargs.iter().find(|(key, _)| key == field)?;
        Some(to_span(&self.ast, *span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// How many calls [`Parsed::new`] visited on this thread.
        pub(super) static CALLS_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn located(src: &str, call: (u32, u32), name: &str, field: &str) -> Option<Span> {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("BUILD"), src).expect("write");
        let index = index(dir.path(), "BUILD").expect("parses");
        let source = SpecSource::checked(
            Arc::from("BUILD"),
            Span {
                start_line: call.0,
                start_col: call.1,
                end_line: call.0,
                end_col: call.1 + 1,
            },
        )
        .expect("source");
        index.locate(&source, name, field)
    }

    const SRC: &str = "\
target(name = \"a\", driver = \"bash\")
target(
    name = \"b\",
    deps = [\"//x\"],
)
";

    #[test]
    fn finds_the_kwarg_of_the_call_at_the_source() {
        let deps = located(SRC, (2, 1), "b", "deps").expect("deps");
        assert_eq!(
            (deps.start_line, deps.start_col, deps.end_line, deps.end_col),
            (4, 5, 4, 19),
            "`deps = [\"//x\"]`, its end exclusive"
        );
        let driver = located(SRC, (1, 1), "a", "driver").expect("driver");
        assert_eq!((driver.start_line, driver.start_col), (1, 20));
    }

    #[test]
    fn no_span_off_the_call() {
        assert_eq!(
            located(SRC, (2, 1), "b", "driver"),
            None,
            "an option not set"
        );
        assert_eq!(
            located(SRC, (3, 5), "b", "deps"),
            None,
            "no call starts there"
        );
        assert_eq!(located(SRC, (2, 1), "a", "deps"), None, "another name");
    }

    #[test]
    fn a_computed_name_is_not_checked() {
        let src = "def m(name):\n    target(name = name, deps = [])\n";
        assert!(located(src, (2, 5), "anything", "deps").is_some());
    }

    /// Only a `target(...)` call's arguments are a spec's options: at a plugin
    /// function's call (the span of a target it declared), the option is not
    /// looked up among the function's own arguments.
    #[test]
    fn only_a_target_call_is_searched() {
        let src = "heph.go.library(name = \"b\", deps = [])\nlib(name = \"b\", deps = [])\n";
        assert_eq!(located(src, (1, 1), "b", "deps"), None, "a plugin function");
        assert_eq!(located(src, (2, 1), "b", "deps"), None, "a macro's call");
        let nested = "f(1)(target(name = \"b\", deps = []))\ntarget(name = \"c\", deps = [])\n";
        assert!(
            located(nested, (2, 1), "c", "deps").is_some(),
            "a target call is found among others"
        );
    }

    /// The file is walked once, when the index is built: a file of 2,000
    /// calls located 2,000 times visits each call once, not once per locate.
    #[test]
    fn locate_is_a_lookup() {
        let n = 2_000u32;
        let src: String = (0..n)
            .map(|i| format!("target(name = \"t{i}\", deps = [])\n"))
            .collect();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("BUILD"), &src).expect("write");
        let before = CALLS_VISITED.with(std::cell::Cell::get);
        let index = index(dir.path(), "BUILD").expect("parses");
        let built = CALLS_VISITED.with(std::cell::Cell::get) - before;
        assert_eq!(built, 2_000, "one visit per call");
        for (i, text) in (0..n).zip(src.lines()) {
            let line = i + 1;
            let col = u32::try_from(text.find("deps").expect("deps") + 1).expect("col");
            let call = Span {
                start_line: line,
                start_col: 1,
                end_line: line,
                end_col: 2,
            };
            let source = SpecSource::checked(Arc::from("BUILD"), call).expect("source");
            let deps = index
                .locate(&source, &format!("t{i}"), "deps")
                .expect("deps");
            assert_eq!((deps.start_line, deps.start_col), (line, col));
        }
        assert_eq!(
            CALLS_VISITED.with(std::cell::Cell::get) - before,
            built,
            "no locate walks the file"
        );
    }

    #[test]
    fn a_file_that_no_longer_parses_has_no_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("BUILD"), "target(").expect("write");
        assert!(index(dir.path(), "BUILD").is_none());
        assert!(index(dir.path(), "gone").is_none());
    }
}
