//! The provider's [`SourceIndex`]: a BUILD or `.bzl` file parsed again,
//! never evaluated, on a diagnostic's error path, to find one keyword
//! argument of the `target()` call a spec's [`SpecSource`] names (D3).

use crate::pluginbuildfile::ast::walk_expr;
use hcore::diagnostic::Span;
use hplugin::provider::{Located, SourceIndex, SpecSource};
use starlark::syntax::AstModule;
use starlark::syntax::ast::{Argument, AstLiteral, Expr};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// `file` (workspace-relative, under `root`) parsed as it is on disk now.
/// When it is gone or no longer parses, every call in it changed since it
/// was evaluated ([`Gone`]).
pub(crate) fn index(root: &Path, file: &str) -> Arc<dyn SourceIndex> {
    match crate::pluginbuildfile::run_file::parse_build_file(&root.join(file)) {
        Ok(ast) => Arc::new(Parsed::new(ast)),
        Err(e) => {
            tracing::debug!(file, error = %e, "re-parsing for a diagnostic");
            Arc::new(Gone)
        }
    }
}

/// A file that no longer reads or parses: no call is where it evaluated.
struct Gone;

impl SourceIndex for Gone {
    fn locate(&self, _source: &SpecSource, _name: &str, _field: Option<&str>) -> Located {
        Located::Changed
    }
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
    fn locate(&self, source: &SpecSource, name: &str, field: Option<&str>) -> Located {
        let at = source.call();
        let (literal, kwargs) = match self.calls.get(&(at.start_line, at.start_col)) {
            // No call starts there any more.
            None => return Located::Changed,
            // A plugin function's call, or a `target` reached under another
            // name: there, but its arguments are not the spec's options.
            Some(Call::Other) => return Located::Call,
            Some(Call::Target { name, kwargs }) => (name, kwargs),
        };
        // The call that was there at eval, or the file changed since: a
        // literal `name=` must still be the target's. A computed one (a
        // macro's `name = name`) is not checked.
        if literal.as_deref().is_some_and(|n| n != name) {
            return Located::Changed;
        }
        field
            .and_then(|field| kwargs.iter().find(|(key, _)| key == field))
            .map_or(Located::Call, |(_, span)| {
                Located::Field(to_span(&self.ast, *span))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// How many calls [`Parsed::new`] visited on this thread.
        pub(super) static CALLS_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn call_at(line: u32, col: u32) -> SpecSource {
        SpecSource::checked(
            Arc::from("BUILD"),
            Span {
                start_line: line,
                start_col: col,
                end_line: line,
                end_col: col + 1,
            },
        )
        .expect("source")
    }

    fn located(src: &str, call: (u32, u32), name: &str, field: &str) -> Located {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("BUILD"), src).expect("write");
        index(dir.path(), "BUILD").locate(&call_at(call.0, call.1), name, Some(field))
    }

    fn field(l: Located) -> Span {
        match l {
            Located::Field(span) => span,
            other => panic!("an option's span, got {other:?}"),
        }
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
        let deps = field(located(SRC, (2, 1), "b", "deps"));
        assert_eq!(
            (deps.start_line, deps.start_col, deps.end_line, deps.end_col),
            (4, 5, 4, 19),
            "`deps = [\"//x\"]`, its end exclusive"
        );
        let driver = field(located(SRC, (1, 1), "a", "driver"));
        assert_eq!((driver.start_line, driver.start_col), (1, 20));
    }

    #[test]
    fn the_call_alone_or_changed_off_the_option() {
        assert_eq!(
            located(SRC, (2, 1), "b", "driver"),
            Located::Call,
            "an option not set"
        );
        assert_eq!(
            located(SRC, (3, 5), "b", "deps"),
            Located::Changed,
            "no call starts there"
        );
        assert_eq!(
            located(SRC, (2, 1), "a", "deps"),
            Located::Changed,
            "another name"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("BUILD"), SRC).expect("write");
        assert_eq!(
            index(dir.path(), "BUILD").locate(&call_at(2, 1), "b", None),
            Located::Call,
            "no option asked for: the call, still there"
        );
        assert_eq!(
            index(dir.path(), "BUILD").locate(&call_at(9, 1), "b", None),
            Located::Changed,
            "no option asked for, and the call moved"
        );
    }

    #[test]
    fn a_computed_name_is_not_checked() {
        let src = "def m(name):\n    target(name = name, deps = [])\n";
        field(located(src, (2, 5), "anything", "deps"));
    }

    /// Only a `target(...)` call's arguments are a spec's options: at a plugin
    /// function's call (the span of a target it declared), the option is not
    /// looked up among the function's own arguments, and the file has not
    /// changed for it.
    #[test]
    fn only_a_target_call_is_searched() {
        let src = "heph.go.library(name = \"b\", deps = [])\nlib(name = \"b\", deps = [])\n";
        assert_eq!(
            located(src, (1, 1), "b", "deps"),
            Located::Call,
            "a plugin function"
        );
        assert_eq!(
            located(src, (2, 1), "b", "deps"),
            Located::Call,
            "a macro's call"
        );
        let nested = "f(1)(target(name = \"b\", deps = []))\ntarget(name = \"c\", deps = [])\n";
        field(located(nested, (2, 1), "c", "deps"));
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
        let index = index(dir.path(), "BUILD");
        let built = CALLS_VISITED.with(std::cell::Cell::get) - before;
        assert_eq!(built, 2_000, "one visit per call");
        for (i, text) in (0..n).zip(src.lines()) {
            let line = i + 1;
            let col = u32::try_from(text.find("deps").expect("deps") + 1).expect("col");
            let deps = field(index.locate(&call_at(line, 1), &format!("t{i}"), Some("deps")));
            assert_eq!((deps.start_line, deps.start_col), (line, col));
        }
        assert_eq!(
            CALLS_VISITED.with(std::cell::Cell::get) - before,
            built,
            "no locate walks the file"
        );
    }

    /// A file that no longer parses, or is gone, still indexes: every call in
    /// it changed since evaluation, so the diagnostic can say so.
    #[test]
    fn a_file_that_no_longer_parses_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("BUILD"), "target(").expect("write");
        for file in ["BUILD", "gone"] {
            assert_eq!(
                index(dir.path(), file).locate(&call_at(1, 1), "a", Some("deps")),
                Located::Changed,
                "{file}"
            );
        }
    }

    /// Columns count chars, not bytes: a multibyte char or a tab is one
    /// column, both before an option and before the call itself (the key
    /// the call is looked up by). CRLF line ends move nothing.
    #[test]
    fn columns_count_chars() {
        let start = |l: Located| {
            let s = field(l);
            (s.start_line, s.start_col, s.end_line, s.end_col)
        };
        // `é` is two bytes: `driver` is at char 20, byte 21.
        let multibyte = "target(name = \"é\", driver = \"bash\")\n";
        assert_eq!(
            start(located(multibyte, (1, 1), "é", "driver")),
            (1, 20, 1, 35)
        );
        // The call itself starts at char 10, byte 11.
        let before_call = "X = \"é\"; target(name = \"t\", driver = \"bash\")\n";
        assert_eq!(
            start(located(before_call, (1, 10), "t", "driver")).1,
            29,
            "found by its char column"
        );
        assert_eq!(
            located(before_call, (1, 11), "t", "driver"),
            Located::Changed,
            "a byte column names no call"
        );
        // Starlark refuses a tab outside a string, so the one tab a file
        // that evaluated can hold before an option is inside one.
        let tab = "target(name = \"\t\", driver = \"bash\")\n";
        assert_eq!(start(located(tab, (1, 1), "\t", "driver")).1, 20);
        let crlf = SRC.replace('\n', "\r\n");
        assert_eq!(
            start(located(&crlf, (2, 1), "b", "deps")),
            (4, 5, 4, 19),
            "as with LF ends"
        );
    }
}
