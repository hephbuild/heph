//! The provider's [`SourceIndex`]: a BUILD or `.bzl` file parsed again,
//! never evaluated, on a diagnostic's error path, to find one keyword
//! argument of the `target()` call a spec's [`SpecSource`] names (D3).

use crate::pluginbuildfile::ast::walk_expr;
use hcore::diagnostic::Span;
use hplugin::provider::{SourceIndex, SpecSource};
use starlark::syntax::AstModule;
use starlark::syntax::ast::{Argument, AstLiteral, Expr};
use std::path::Path;
use std::sync::Arc;

/// `file` (workspace-relative, under `root`) parsed as it is on disk now, or
/// `None` when it is gone or no longer parses: every diagnostic then points
/// at its call.
pub(crate) fn index(root: &Path, file: &str) -> Option<Arc<dyn SourceIndex>> {
    let ast = crate::pluginbuildfile::run_file::parse_build_file(&root.join(file))
        .inspect_err(|e| tracing::debug!(file, error = %e, "re-parsing for a diagnostic"))
        .ok()?;
    Some(Arc::new(Parsed { ast }))
}

struct Parsed {
    ast: AstModule,
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
        let mut found = None;
        self.ast.statement().visit_expr(|top| {
            walk_expr(top, &mut |e| {
                if found.is_some() {
                    return;
                }
                let Expr::Call(_, args) = &e.node else {
                    return;
                };
                let span = to_span(&self.ast, e.span);
                if (span.start_line, span.start_col) != (at.start_line, at.start_col) {
                    return;
                }
                // The call that was there at eval, or the file changed since:
                // a literal `name=` must still be the target's. A computed one
                // (a macro's `name = name`) is not checked.
                let mut kwarg = None;
                for arg in &args.args {
                    let Argument::Named(key, value) = &arg.node else {
                        continue;
                    };
                    if key.node == "name"
                        && let Expr::Literal(AstLiteral::String(s)) = &value.node
                        && s.node != name
                    {
                        found = Some(None);
                        return;
                    }
                    if key.node == field {
                        kwarg = Some(to_span(&self.ast, key.span.merge(value.span)));
                    }
                }
                found = Some(kwarg);
            });
        });
        found.flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_file_that_no_longer_parses_has_no_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("BUILD"), "target(").expect("write");
        assert!(index(dir.path(), "BUILD").is_none());
        assert!(index(dir.path(), "gone").is_none());
    }
}
