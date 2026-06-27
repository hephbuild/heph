//! A formatter for Starlark `BUILD` files.
//!
//! Ported from the heph v0 (Go) `utils/xstarlark/fmt` package. The Go reference
//! relies on go.starlark.net's `RetainComments`, which attaches comments to AST
//! nodes; the Rust `starlark` parser instead exposes comments only as a flat
//! list of spans (`AstModule::comments()`), so this crate re-associates them to
//! nodes by source position (see [`comments`]).

mod comments;
mod format;
mod printer;
mod quote;

use crate::comments::CommentStore;
use crate::format::Formatter;
use crate::printer::Printer;
use starlark::codemap::CodeMap;
use starlark::syntax::AstModule;
use starlark::syntax::Dialect;
use starlark::syntax::ast::AstStmt;
use starlark::syntax::ast::StmtP;
use starlark_syntax::syntax::module::AstModuleFields;

/// The directive that, when present as a leading comment, skips formatting.
pub const COMMENT_SKIP_FILE: &str = "heph:fmt skip-file";

/// Formatter configuration.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Number of spaces per indentation level. Must be greater than zero.
    pub indent_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config { indent_size: 2 }
    }
}

/// Errors returned by [`format`].
#[derive(Debug)]
pub enum FmtError {
    /// The file opens with the `heph:fmt skip-file` directive and was left
    /// untouched.
    Skip,
    /// Invalid configuration (e.g. a non-positive indent size).
    Config(String),
    /// The source could not be parsed as Starlark.
    Parse(anyhow::Error),
}

impl std::fmt::Display for FmtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FmtError::Skip => write!(f, "file formatting skipped"),
            FmtError::Config(m) => write!(f, "invalid formatter config: {m}"),
            FmtError::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

impl std::error::Error for FmtError {}

/// Format Starlark source, returning the formatted text.
///
/// `filename` is used only for error messages. Returns [`FmtError::Skip`] when
/// the file begins with the `heph:fmt skip-file` directive.
pub fn format(filename: &str, src: &str, config: Config) -> Result<String, FmtError> {
    if config.indent_size == 0 {
        return Err(FmtError::Config("indent must be > 0".to_string()));
    }

    let module = AstModule::parse(filename, src.to_string(), &Dialect::Extended)
        .map_err(|e| FmtError::Parse(e.into_anyhow()))?;

    let codemap = module.codemap();
    let comment_spans = module.comments();
    let stmts = top_level_stmts(module.statement());

    if stmts
        .first()
        .is_some_and(|first| starts_with_skip(codemap, comment_spans, first))
    {
        return Err(FmtError::Skip);
    }

    let store = CommentStore::new(codemap, comment_spans);
    let mut printer = Printer::new(" ".repeat(config.indent_size));
    let mut formatter = Formatter::new(codemap, store, " ".repeat(config.indent_size));
    formatter.format_file(&mut printer, stmts);

    Ok(printer.into_string())
}

/// Top-level statements: the children of the synthetic `Statements` node that
/// wraps a parsed module, or the single statement itself.
fn top_level_stmts(stmt: &AstStmt) -> &[AstStmt] {
    match &stmt.node {
        StmtP::Statements(v) => v,
        _ => std::slice::from_ref(stmt),
    }
}

/// Whether the file's leading comments (above the first statement) contain the
/// skip-file directive.
fn starts_with_skip(codemap: &CodeMap, spans: &[starlark::codemap::Span], first: &AstStmt) -> bool {
    let first_line = codemap.resolve_span(first.span).begin.line;
    spans.iter().any(|&span| {
        let line = codemap.resolve_span(span).begin.line;
        line < first_line && codemap.source_span(span).contains(COMMENT_SKIP_FILE)
    })
}
