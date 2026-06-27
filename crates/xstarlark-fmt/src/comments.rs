//! Comment re-association.
//!
//! The Rust `starlark` parser discards comment tokens before building the AST,
//! exposing them only as a flat, source-ordered list of spans via
//! `AstModule::comments()`. The Go reference instead gets comments attached to
//! each node (`.Before`/`.Suffix`/`.After`) for free.
//!
//! We bridge the gap with a position-driven cursor: comments are resolved to
//! `(line, col, text)` once, then emitted lazily as the formatter walks the AST
//! in source order. Standalone comments are flushed above the node they precede
//! (`flush_before`), same-line comments become suffixes (`take_suffix`), and
//! trailing comments are flushed either at a bracket close (`flush_list_close`,
//! line-gated) or at a block dedent (`flush_block`, column-gated — the source
//! column cleanly partitions nested trailing comments by depth).

use crate::printer::Printer;
use crate::quote::format_comment;
use starlark::codemap::CodeMap;
use starlark::codemap::Span;

struct Comment {
    line: usize,
    col: usize,
    text: String,
}

pub(crate) struct CommentStore {
    items: Vec<Comment>,
    next: usize,
    /// When measuring element widths we re-format expressions into a scratch
    /// buffer; comment emission must be suppressed and must not consume.
    suppressed: bool,
}

impl CommentStore {
    pub(crate) fn new(codemap: &CodeMap, spans: &[Span]) -> Self {
        let mut items: Vec<Comment> = spans
            .iter()
            .map(|&span| {
                let resolved = codemap.resolve_span(span);
                Comment {
                    line: resolved.begin.line,
                    col: resolved.begin.column,
                    text: codemap.source_span(span).to_string(),
                }
            })
            .collect();
        items.sort_by(|a, b| a.line.cmp(&b.line).then(a.col.cmp(&b.col)));
        CommentStore {
            items,
            next: 0,
            suppressed: false,
        }
    }

    pub(crate) fn set_suppressed(&mut self, suppressed: bool) -> bool {
        let prev = self.suppressed;
        self.suppressed = suppressed;
        prev
    }

    fn peek(&self) -> Option<&Comment> {
        self.items.get(self.next)
    }

    /// Line of the next pending comment, if any.
    pub(crate) fn next_line(&self) -> Option<usize> {
        self.peek().map(|c| c.line)
    }

    /// Whether any pending comment falls on a line in `(after, before)`
    /// (exclusive of `after`, exclusive of `before`). Non-consuming.
    pub(crate) fn has_between(&self, after: usize, before: usize) -> bool {
        self.items
            .iter()
            .skip(self.next)
            .any(|c| c.line > after && c.line < before)
    }

    /// Whether *any* comment (consumed or not) falls on a line strictly inside
    /// `(after, before)`. Used to detect calls that carry an inner comment.
    pub(crate) fn any_line_between(&self, after: usize, before: usize) -> bool {
        self.items.iter().any(|c| c.line > after && c.line < before)
    }

    /// Emit the next pending comment, advancing the cursor. A blank line is
    /// inserted first when this comment was separated from the previously-emitted
    /// one (`prev`) by a blank line in the source — so the grouping of adjacent
    /// comment blocks (e.g. commented-out alternatives) is preserved.
    fn emit_next(&mut self, p: &mut Printer, prev: &mut Option<usize>) {
        let Some(c) = self.items.get(self.next) else {
            return;
        };
        let line = c.line;
        let text = format_comment(&c.text);
        if prev.is_some_and(|pl| line > pl + 1) {
            p.write("\n");
        }
        p.write(&text);
        p.write("\n");
        *prev = Some(line);
        self.next += 1;
    }

    /// Seed the blank-line tracker for a trailing flush: a blank is wanted
    /// before the first trailing comment when it is separated from the preceding
    /// statement (`anchor`) — or the most recently emitted comment, whichever is
    /// later in the source — by a blank line.
    fn seed_prev(&self, anchor: Option<usize>) -> Option<usize> {
        let last_emitted = self
            .next
            .checked_sub(1)
            .and_then(|j| self.items.get(j))
            .map(|c| c.line);
        match (anchor, last_emitted) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Emit, each on its own line, all pending comments strictly above `line`.
    /// Returns the source line of the last comment emitted, if any (so a caller
    /// can preserve a blank line between a leading comment and its statement).
    pub(crate) fn flush_before(&mut self, p: &mut Printer, line: usize) -> Option<usize> {
        if self.suppressed {
            return None;
        }
        let mut prev = None;
        loop {
            let go = matches!(self.peek(), Some(c) if c.line < line);
            if !go {
                break;
            }
            self.emit_next(p, &mut prev);
        }
        prev
    }

    /// If the next pending comment is on `line`, consume it and return its
    /// normalized text (without trailing newline) for use as a suffix.
    pub(crate) fn take_suffix(&mut self, line: usize) -> Option<String> {
        if self.suppressed {
            return None;
        }
        if let Some(c) = self.peek()
            && c.line == line
        {
            let text = format_comment(&c.text);
            self.next += 1;
            return Some(text);
        }
        None
    }

    /// Emit all pending comments above `close_line` — trailing comments inside a
    /// bracketed list/call/dict before its closing delimiter.
    pub(crate) fn flush_list_close(&mut self, p: &mut Printer, close_line: usize) {
        self.flush_before(p, close_line);
    }

    /// Emit pending comments indented at or deeper than `body_col` — the
    /// trailing comments belonging to a block at this nesting level. When
    /// `before_line` is set, only comments above that line are flushed (so a
    /// block's trailing comments don't reach into a following `elif`/`else`).
    /// `anchor` is the preceding statement's line, so a blank line between it and
    /// the first trailing comment is preserved.
    pub(crate) fn flush_block(
        &mut self,
        p: &mut Printer,
        body_col: usize,
        before_line: Option<usize>,
        anchor: Option<usize>,
    ) {
        if self.suppressed {
            return;
        }
        let mut prev = self.seed_prev(anchor);
        loop {
            let go = matches!(
                self.peek(),
                Some(c) if c.col >= body_col && before_line.is_none_or(|bl| c.line < bl)
            );
            if !go {
                break;
            }
            self.emit_next(p, &mut prev);
        }
    }

    /// Emit every remaining comment (end-of-file trailing comments). `anchor` is
    /// the last statement's line, so a blank line before the first trailing
    /// comment is preserved.
    pub(crate) fn flush_rest(&mut self, p: &mut Printer, anchor: Option<usize>) {
        if self.suppressed {
            return;
        }
        let mut prev = self.seed_prev(anchor);
        while self.peek().is_some() {
            self.emit_next(p, &mut prev);
        }
    }
}
