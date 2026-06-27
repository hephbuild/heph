//! The AST walk that produces formatted output. Port of the Go reference's
//! `fmt.go` (statement/expression rendering, list-wrapping heuristics,
//! blank-line rules) adapted to the `starlark` Rust AST. Comment placement is
//! delegated to [`CommentStore`].

use crate::comments::CommentStore;
use crate::printer::Printer;
use crate::quote::quote_literal;
use starlark::codemap::CodeMap;
use starlark::codemap::Span;
use starlark::syntax::ast::ArgumentP;
use starlark::syntax::ast::AssignTargetP;
use starlark::syntax::ast::AstArgument;
use starlark::syntax::ast::AstAssignTarget;
use starlark::syntax::ast::AstExpr;
use starlark::syntax::ast::AstParameter;
use starlark::syntax::ast::AstStmt;
use starlark::syntax::ast::AstTypeExpr;
use starlark::syntax::ast::ClauseP;
use starlark::syntax::ast::ExprP;
use starlark::syntax::ast::ForClauseP;
use starlark::syntax::ast::LambdaP;
use starlark::syntax::ast::ParameterP;
use starlark::syntax::ast::StmtP;

const LIST_MAX_LINE_LENGTH: usize = 100;
const LIST_MAX_COMBINED_LENGTH: usize = 50;
const LIST_MAX_ELEM_LENGTH: usize = 50;

// Expression binding precedence (higher binds tighter). A sub-expression whose
// precedence is below its context's required minimum is parenthesized.
const PREC_MIN: u8 = 0; // statement / argument / bracketed: never parenthesize
const PREC_COND: u8 = 1; // `x if c else y`, `lambda`
const PREC_OR: u8 = 2;
const PREC_AND: u8 = 3;
const PREC_NOT: u8 = 4;
const PREC_CMP: u8 = 5; // == != < > <= >= in, not in
const PREC_BIT_OR: u8 = 6;
const PREC_BIT_XOR: u8 = 7;
const PREC_BIT_AND: u8 = 8;
const PREC_SHIFT: u8 = 9;
const PREC_ADD: u8 = 10; // + -
const PREC_MUL: u8 = 11; // * / // %
const PREC_UNARY: u8 = 12; // unary - + ~
const PREC_ATOM: u8 = 13; // literals, names, calls, indexing, lists, …

/// Binding precedence of a binary operator.
fn binop_prec(op: &starlark::syntax::ast::BinOp) -> u8 {
    use starlark::syntax::ast::BinOp;
    match op {
        BinOp::Or => PREC_OR,
        BinOp::And => PREC_AND,
        BinOp::Equal
        | BinOp::NotEqual
        | BinOp::Less
        | BinOp::Greater
        | BinOp::LessOrEqual
        | BinOp::GreaterOrEqual
        | BinOp::In
        | BinOp::NotIn => PREC_CMP,
        BinOp::BitOr => PREC_BIT_OR,
        BinOp::BitXor => PREC_BIT_XOR,
        BinOp::BitAnd => PREC_BIT_AND,
        BinOp::LeftShift | BinOp::RightShift => PREC_SHIFT,
        BinOp::Add | BinOp::Subtract => PREC_ADD,
        BinOp::Multiply | BinOp::Divide | BinOp::FloorDivide | BinOp::Percent => PREC_MUL,
    }
}

/// Binding precedence of an expression — how tightly it holds together, used to
/// decide whether it needs parentheses in a given context.
fn expr_prec(expr: &ExprP<starlark::syntax::ast::AstNoPayload>) -> u8 {
    match expr {
        ExprP::If(_) | ExprP::Lambda(_) => PREC_COND,
        ExprP::Op(_, op, _) => binop_prec(op),
        ExprP::Not(_) => PREC_NOT,
        ExprP::Minus(_) | ExprP::Plus(_) | ExprP::BitNot(_) => PREC_UNARY,
        _ => PREC_ATOM,
    }
}

pub(crate) struct Formatter<'a> {
    codemap: &'a CodeMap,
    comments: CommentStore,
    indent_unit: String,
}

/// A single item rendered between brackets (a call arg, list/tuple element,
/// dict entry, or def param). Copyable so it can be handed to the measurement
/// closure without borrowing the formatter.
#[derive(Clone, Copy)]
enum Item<'x> {
    Arg(&'x AstArgument),
    Expr(&'x AstExpr),
    DictEntry(&'x AstExpr, &'x AstExpr),
    Param(&'x AstParameter),
}

impl Item<'_> {
    fn span(&self) -> Span {
        match self {
            Item::Arg(a) => a.span,
            Item::Expr(e) => e.span,
            Item::DictEntry(k, v) => k.span.merge(v.span),
            Item::Param(p) => p.span,
        }
    }

    fn render(self, f: &mut Formatter, p: &mut Printer) {
        match self {
            Item::Arg(a) => f.format_argument(p, a),
            Item::Expr(e) => f.format_expr(p, e),
            Item::DictEntry(k, v) => {
                f.format_expr(p, k);
                p.write(": ");
                f.format_expr(p, v);
            }
            Item::Param(pa) => f.format_param(p, pa),
        }
    }
}

impl<'a> Formatter<'a> {
    pub(crate) fn new(codemap: &'a CodeMap, comments: CommentStore, indent_unit: String) -> Self {
        Formatter {
            codemap,
            comments,
            indent_unit,
        }
    }

    pub(crate) fn format_file(&mut self, p: &mut Printer, stmts: &[AstStmt]) {
        self.format_stmt_seq(p, stmts, None);
        let anchor = stmts.last().map(|s| self.end_line(s.span));
        self.comments.flush_rest(p, anchor);
    }

    // --- position helpers -------------------------------------------------

    fn start_line(&self, span: Span) -> usize {
        self.codemap.resolve_span(span).begin.line
    }

    fn end_line(&self, span: Span) -> usize {
        self.codemap.resolve_span(span).end.line
    }

    fn start_col(&self, span: Span) -> usize {
        self.codemap.resolve_span(span).begin.column
    }

    /// 0-based line of the first `ch` in the source byte range `[after, before)`.
    fn line_of_char(&self, after: usize, before: usize, ch: char) -> Option<usize> {
        let src = self.codemap.source();
        let slice = src.get(after..before)?;
        let rel = slice.find(ch)?;
        let abs = after + rel;
        Some(src.bytes().take(abs).filter(|&b| b == b'\n').count())
    }

    // --- statements -------------------------------------------------------

    /// Format a sequence of statements. `seq_bound` is the line past which this
    /// sequence's last statement may not pull trailing comments (the next
    /// statement of the enclosing block, or `None` at end-of-file).
    fn format_stmt_seq(&mut self, p: &mut Printer, stmts: &[AstStmt], seq_bound: Option<usize>) {
        let top = p.level() == 0;
        for (i, stmt) in stmts.iter().enumerate() {
            let bare_start = self.start_line(stmt.span);
            if let Some(prev) = i.checked_sub(1).and_then(|j| stmts.get(j)) {
                let eff_start = match self.comments.next_line() {
                    Some(l) if l < bare_start => l,
                    _ => bare_start,
                };
                let prev_end = self.end_line(prev.span);
                if self.skip_line(top, NodeRef::Stmt(prev), stmt, eff_start, prev_end) {
                    p.write("\n");
                }
            }
            // Preserve a blank line between a leading comment and its statement.
            if let Some(last_comment) = self.comments.flush_before(p, bare_start)
                && last_comment + 1 < bare_start
            {
                p.write("\n");
            }
            // A statement's trailing comments reach no further than the start of
            // the next statement (or the sequence bound for the last one) — so a
            // block never absorbs a comment that belongs to later code at the
            // same indentation.
            let trailing_bound = match stmts.get(i + 1) {
                Some(next) => Some(self.start_line(next.span)),
                None => seq_bound,
            };
            self.format_stmt(p, stmt, trailing_bound);
        }
    }

    /// Statements of a nested block (def/if/for body), with trailing block
    /// comments flushed at this nesting level before dedenting. `before_line`
    /// bounds the trailing flush so a branch body does not absorb comments that
    /// belong to a following `elif`/`else` or the enclosing block.
    fn format_block(&mut self, p: &mut Printer, stmts: &[AstStmt], before_line: Option<usize>) {
        p.push();
        self.format_stmt_seq(p, stmts, before_line);
        if let Some(first) = stmts.first() {
            let col = self.start_col(first.span);
            let anchor = stmts.last().map(|s| self.end_line(s.span));
            self.comments.flush_block(p, col, before_line, anchor);
        }
        p.pop();
    }

    fn format_stmt(&mut self, p: &mut Printer, stmt: &AstStmt, trailing_bound: Option<usize>) {
        match &stmt.node {
            StmtP::If(_, _) | StmtP::IfElse(_, _) => {
                self.format_if(p, stmt, trailing_bound);
                return;
            }
            StmtP::For(f) => {
                let header_end = self.end_line(f.over.span);
                p.write("for ");
                self.format_assign_target(p, &f.var);
                p.write(" in ");
                self.format_expr(p, &f.over);
                let _ = self.comments.take_suffix(header_end);
                p.write(":\n");
                self.format_block(p, block_stmts(&f.body), trailing_bound);
                return;
            }
            StmtP::Def(d) => {
                p.write("def ");
                p.write(&d.name.node.ident);
                let close_line = self.params_close_line(stmt.span, &d.params, &d.body);
                let open_line = self.start_line(stmt.span);
                let params: Vec<Item> = d.params.iter().map(Item::Param).collect();
                self.format_list(p, &params, (open_line, close_line), false, ("(", ")"));
                if let Some(rt) = &d.return_type {
                    p.write(" -> ");
                    self.format_type(p, rt);
                }
                let _ = self.comments.take_suffix(close_line);
                p.write(":\n");
                self.format_block(p, block_stmts(&d.body), trailing_bound);
                return;
            }
            StmtP::Load(l) => {
                p.write("load(");
                p.write(&quote_literal(&l.module.node, self.raw(l.module.span)));
                for arg in &l.args {
                    p.write(", ");
                    let local = &arg.local.node.ident;
                    let their = &arg.their.node;
                    if local == their {
                        p.write(&quote_literal(their, self.raw(arg.their.span)));
                    } else {
                        p.write(local);
                        p.write(" = ");
                        p.write(&quote_literal(their, self.raw(arg.their.span)));
                    }
                }
                p.write(")");
            }
            StmtP::Expression(x) => {
                self.format_expr(p, x);
            }
            StmtP::Assign(a) => {
                self.format_assign_target(p, &a.lhs);
                if let Some(ty) = &a.ty {
                    p.write(": ");
                    self.format_type(p, ty);
                }
                p.write(" = ");
                self.format_expr(p, &a.rhs);
            }
            StmtP::AssignModify(target, op, rhs) => {
                self.format_assign_target(p, target);
                p.write(&format!("{op}"));
                self.format_expr(p, rhs);
            }
            StmtP::Return(val) => {
                if let Some(v) = val {
                    p.write("return ");
                    self.format_expr(p, v);
                } else {
                    p.write("return");
                }
            }
            StmtP::Break => p.write("break"),
            StmtP::Continue => p.write("continue"),
            StmtP::Pass => p.write("pass"),
            StmtP::Statements(inner) => {
                // A bare nested statement block; render its contents in place.
                self.format_stmt_seq(p, inner, trailing_bound);
                return;
            }
        }

        // Simple (single-line) statement: emit an optional same-line suffix
        // comment, then terminate the line.
        let end = self.end_line(stmt.span);
        if let Some(suffix) = self.comments.take_suffix(end) {
            p.write(" ");
            p.write(&suffix);
        }
        if !p.ends_with_newline() {
            p.write("\n");
        }
    }

    /// Resugar `if` / nested `else: if` chains into `if`/`elif`/`else`.
    /// `trailing_bound` bounds the final branch / `else` body's trailing
    /// comments (the next statement after the whole `if`).
    fn format_if(&mut self, p: &mut Printer, stmt: &AstStmt, trailing_bound: Option<usize>) {
        let mut branches: Vec<(&AstExpr, &AstStmt)> = Vec::new();
        let mut else_block: Option<&AstStmt> = None;

        match &stmt.node {
            StmtP::If(cond, body) => branches.push((cond, body)),
            StmtP::IfElse(cond, bx) => {
                let (then, els) = &**bx;
                branches.push((cond, then));
                else_block = Some(els);
            }
            // The caller only dispatches here for `If`/`IfElse`.
            _ => return,
        }

        // Flatten `else: if …` chains into `elif` branches.
        while let Some(eb) = else_block {
            match block_stmts(eb) {
                [single] => match &single.node {
                    StmtP::If(c, b) => {
                        branches.push((c, b));
                        else_block = None;
                    }
                    StmtP::IfElse(c, bx) => {
                        let (t, e) = &**bx;
                        branches.push((c, t));
                        else_block = Some(e);
                    }
                    _ => break,
                },
                _ => break,
            }
        }

        // First content line of the else block, if any — used both as the
        // bound for the final branch body and to flush comments before `else`.
        let else_first_line = else_block
            .and_then(|eb| block_stmts(eb).first())
            .map(|s| self.start_line(s.span));

        for (i, (cond, body)) in branches.iter().enumerate() {
            // Comments sitting between the previous branch and this `elif`
            // belong at the if-statement's own indent level.
            if i > 0 {
                self.comments.flush_before(p, self.start_line(cond.span));
            }
            p.write(if i == 0 { "if" } else { "elif" });
            p.write(" ");
            self.format_expr(p, cond);
            // The if/elif suffix cannot be faithfully rendered; drop it.
            let _ = self.comments.take_suffix(self.end_line(cond.span));
            p.write(":\n");
            // This body's trailing comments are bounded by the next branch's
            // condition, then the else block, then the statement after the whole
            // `if` (never unbounded — that would swallow later same-indent code).
            let before = match branches.get(i + 1) {
                Some((next_cond, _)) => Some(self.start_line(next_cond.span)),
                None => else_first_line.or(trailing_bound),
            };
            self.format_block(p, block_stmts(body), before);
        }

        if let Some(eb) = else_block {
            if let Some(else_line) = else_first_line {
                self.comments.flush_before(p, else_line);
            }
            p.write("else:\n");
            self.format_block(p, block_stmts(eb), trailing_bound);
        }
    }

    // --- expressions ------------------------------------------------------

    /// Format an expression in a context that imposes no minimum precedence
    /// (a statement, an argument, or a bracketed position).
    fn format_expr(&mut self, p: &mut Printer, expr: &AstExpr) {
        self.format_expr_prec(p, expr, PREC_MIN);
    }

    /// Format an expression that appears where the surrounding operator binds at
    /// `min` precedence, adding parentheses when the expression binds looser —
    /// the Rust `starlark` AST drops source parentheses, so the formatter must
    /// re-insert the ones meaning depends on (e.g. a ternary inside `+`).
    fn format_expr_prec(&mut self, p: &mut Printer, expr: &AstExpr, min: u8) {
        let parens = expr_prec(&expr.node) < min;
        if parens {
            p.write("(");
        }
        self.format_expr_bare(p, expr);
        if parens {
            p.write(")");
        }
    }

    fn format_expr_bare(&mut self, p: &mut Printer, expr: &AstExpr) {
        match &expr.node {
            ExprP::Identifier(id) => p.write(&id.node.ident),
            ExprP::Literal(lit) => self.format_literal(p, lit, expr.span),
            ExprP::Dot(x, name) => {
                self.format_expr_prec(p, x, PREC_ATOM);
                p.write(".");
                p.write(&name.node);
            }
            ExprP::Call(func, args) => {
                self.format_expr_prec(p, func, PREC_ATOM);
                let nl = p.level() == 0 && is_target_like(&args.args);
                let region = (self.start_line(expr.span), self.end_line(expr.span));
                let items: Vec<Item> = args.args.iter().map(Item::Arg).collect();
                self.format_list(p, &items, region, nl, ("(", ")"));
            }
            ExprP::Index(bx) => {
                let (x, i) = &**bx;
                self.format_expr_prec(p, x, PREC_ATOM);
                p.write("[");
                self.format_expr(p, i);
                p.write("]");
            }
            ExprP::Index2(bx) => {
                let (x, i, j) = &**bx;
                self.format_expr_prec(p, x, PREC_ATOM);
                p.write("[");
                self.format_expr(p, i);
                p.write(", ");
                self.format_expr(p, j);
                p.write("]");
            }
            ExprP::Slice(x, a, b, c) => {
                self.format_expr_prec(p, x, PREC_ATOM);
                p.write("[");
                if let Some(a) = a {
                    self.format_expr(p, a);
                }
                p.write(":");
                if let Some(b) = b {
                    self.format_expr(p, b);
                }
                if let Some(c) = c {
                    p.write(":");
                    self.format_expr(p, c);
                }
                p.write("]");
            }
            ExprP::Op(l, op, r) => {
                let pr = binop_prec(op);
                // Binary operators are left-associative: the left operand may
                // bind at the same precedence, the right must bind tighter.
                self.format_expr_prec(p, l, pr);
                p.write(&format!("{op}"));
                self.format_expr_prec(p, r, pr + 1);
            }
            ExprP::Not(x) => {
                p.write("not ");
                self.format_expr_prec(p, x, PREC_NOT);
            }
            ExprP::Minus(x) => {
                p.write("-");
                self.format_expr_prec(p, x, PREC_UNARY);
            }
            ExprP::Plus(x) => {
                p.write("+");
                self.format_expr_prec(p, x, PREC_UNARY);
            }
            ExprP::BitNot(x) => {
                p.write("~");
                self.format_expr_prec(p, x, PREC_UNARY);
            }
            ExprP::If(bx) => {
                let (cond, v1, v2) = &**bx;
                // `v1 if cond else v2`: the value and condition bind tighter than
                // the conditional; the else-branch is right-associative.
                self.format_expr_prec(p, v1, PREC_COND + 1);
                p.write(" if ");
                self.format_expr_prec(p, cond, PREC_COND + 1);
                p.write(" else ");
                self.format_expr_prec(p, v2, PREC_COND);
            }
            ExprP::List(v) => {
                let region = (self.start_line(expr.span), self.end_line(expr.span));
                let items: Vec<Item> = v.iter().map(Item::Expr).collect();
                self.format_list(p, &items, region, false, ("[", "]"));
            }
            ExprP::Tuple(v) => {
                let region = (self.start_line(expr.span), self.end_line(expr.span));
                let items: Vec<Item> = v.iter().map(Item::Expr).collect();
                self.format_list(p, &items, region, false, ("(", ")"));
            }
            ExprP::Dict(v) => {
                let region = (self.start_line(expr.span), self.end_line(expr.span));
                let items: Vec<Item> = v.iter().map(|(k, val)| Item::DictEntry(k, val)).collect();
                self.format_list(p, &items, region, v.len() > 1, ("{", "}"));
            }
            ExprP::ListComprehension(body, first, clauses) => {
                p.write("[");
                self.format_expr(p, body);
                p.write(" ");
                self.format_for_clause(p, first);
                for clause in clauses {
                    p.write(" ");
                    self.format_clause(p, clause);
                }
                p.write("]");
            }
            ExprP::DictComprehension(bx, first, clauses) => {
                let (k, v) = &**bx;
                p.write("{");
                self.format_expr(p, k);
                p.write(": ");
                self.format_expr(p, v);
                p.write(" ");
                self.format_for_clause(p, first);
                for clause in clauses {
                    p.write(" ");
                    self.format_clause(p, clause);
                }
                p.write("}");
            }
            ExprP::Lambda(l) => self.format_lambda(p, l),
            ExprP::FString(_) => {
                // No structured re-rendering; preserve the source verbatim.
                p.write_raw(self.raw(expr.span));
            }
        }
    }

    fn format_literal(
        &mut self,
        p: &mut Printer,
        lit: &starlark::syntax::ast::AstLiteral,
        span: Span,
    ) {
        use starlark::syntax::ast::AstLiteral;
        match lit {
            AstLiteral::String(s) => {
                p.write_raw(&quote_literal(&s.node, self.raw(s.span)));
            }
            AstLiteral::Ellipsis => p.write("..."),
            // Int / Float / Bytes: preserve the source token verbatim.
            AstLiteral::Int(_) | AstLiteral::Float(_) | AstLiteral::Bytes(_) => {
                p.write_raw(self.raw(span));
            }
        }
    }

    fn format_argument(&mut self, p: &mut Printer, arg: &AstArgument) {
        match &arg.node {
            ArgumentP::Positional(x) => self.format_expr(p, x),
            ArgumentP::Named(name, x) => {
                p.write(&name.node);
                p.write(" = ");
                self.format_expr(p, x);
            }
            ArgumentP::Args(x) => {
                p.write("*");
                self.format_expr(p, x);
            }
            ArgumentP::KwArgs(x) => {
                p.write("**");
                self.format_expr(p, x);
            }
        }
    }

    fn format_param(&mut self, p: &mut Printer, param: &AstParameter) {
        match &param.node {
            ParameterP::Slash => p.write("/"),
            ParameterP::NoArgs => p.write("*"),
            ParameterP::Normal(name, ty, default) => {
                p.write(&name.node.ident);
                if let Some(ty) = ty {
                    p.write(": ");
                    self.format_type(p, ty);
                }
                if let Some(default) = default {
                    p.write(" = ");
                    self.format_expr(p, default);
                }
            }
            ParameterP::Args(name, ty) => {
                p.write("*");
                p.write(&name.node.ident);
                if let Some(ty) = ty {
                    p.write(": ");
                    self.format_type(p, ty);
                }
            }
            ParameterP::KwArgs(name, ty) => {
                p.write("**");
                p.write(&name.node.ident);
                if let Some(ty) = ty {
                    p.write(": ");
                    self.format_type(p, ty);
                }
            }
        }
    }

    fn format_type(&mut self, p: &mut Printer, ty: &AstTypeExpr) {
        self.format_expr(p, &ty.node.expr);
    }

    fn format_assign_target(&mut self, p: &mut Printer, target: &AstAssignTarget) {
        match &target.node {
            AssignTargetP::Identifier(id) => p.write(&id.node.ident),
            AssignTargetP::Tuple(v) => {
                p.write("(");
                for (i, t) in v.iter().enumerate() {
                    if i > 0 {
                        p.write(", ");
                    }
                    self.format_assign_target(p, t);
                }
                p.write(")");
            }
            AssignTargetP::Dot(x, name) => {
                self.format_expr(p, x);
                p.write(".");
                p.write(&name.node);
            }
            AssignTargetP::Index(bx) => {
                let (x, i) = &**bx;
                self.format_expr(p, x);
                p.write("[");
                self.format_expr(p, i);
                p.write("]");
            }
        }
    }

    fn format_for_clause(
        &mut self,
        p: &mut Printer,
        fc: &ForClauseP<starlark::syntax::ast::AstNoPayload>,
    ) {
        p.write("for ");
        self.format_assign_target(p, &fc.var);
        p.write(" in ");
        self.format_expr(p, &fc.over);
    }

    fn format_clause(
        &mut self,
        p: &mut Printer,
        clause: &ClauseP<starlark::syntax::ast::AstNoPayload>,
    ) {
        match clause {
            ClauseP::For(fc) => self.format_for_clause(p, fc),
            ClauseP::If(cond) => {
                p.write("if ");
                self.format_expr(p, cond);
            }
        }
    }

    fn format_lambda(&mut self, p: &mut Printer, l: &LambdaP<starlark::syntax::ast::AstNoPayload>) {
        p.write("lambda");
        for (i, param) in l.params.iter().enumerate() {
            p.write(if i == 0 { " " } else { ", " });
            self.format_param(p, param);
        }
        p.write(": ");
        self.format_expr(p, &l.body);
    }

    // --- list / argument wrapping ----------------------------------------

    fn format_list(
        &mut self,
        p: &mut Printer,
        items: &[Item],
        region: (usize, usize),
        force_nl: bool,
        brackets: (&str, &str),
    ) {
        let (l, r) = brackets;
        let (open_line, close_line) = region;
        let n = items.len();
        let has_inner = self.comments.has_between(open_line, close_line);

        if n == 0 && !has_inner {
            p.write(l);
            p.write(r);
            return;
        }

        let mut nl = force_nl || has_inner;

        if !nl {
            let mut length = 2usize;
            for item in items {
                let s = self.measure(|f, sp| item.render(f, sp));
                let slen = s.chars().count();
                length += slen + 2;
                let over_line = p.line_length() + length > LIST_MAX_LINE_LENGTH
                    && p.line_length() <= LIST_MAX_LINE_LENGTH;
                if s.contains('\n')
                    || (n > 1 && (slen > LIST_MAX_ELEM_LENGTH || length > LIST_MAX_COMBINED_LENGTH))
                    || over_line
                {
                    nl = true;
                    break;
                }
            }
        }

        p.write(l);
        if nl {
            p.push();
            p.write("\n");
            for item in items {
                let span = item.span();
                self.comments.flush_before(p, self.start_line(span));
                item.render(self, p);
                p.write(",");
                if let Some(suffix) = self.comments.take_suffix(self.end_line(span)) {
                    p.write(" ");
                    p.write(&suffix);
                }
                p.write("\n");
            }
            self.comments.flush_list_close(p, close_line);
            p.pop();
        } else {
            for (i, item) in items.iter().enumerate() {
                item.render(self, p);
                if i + 1 != n {
                    p.write(", ");
                }
            }
        }
        p.write(r);
    }

    /// Render an item into a scratch buffer to measure its width, without
    /// consuming or emitting comments.
    fn measure(&mut self, fmt: impl FnOnce(&mut Self, &mut Printer)) -> String {
        let prev = self.comments.set_suppressed(true);
        let mut scratch = Printer::new(self.indent_unit.clone());
        fmt(self, &mut scratch);
        self.comments.set_suppressed(prev);
        scratch.into_string()
    }

    /// 0-based line of the `)` that closes a def parameter list, used to split
    /// param-trailing comments from body-leading comments.
    fn params_close_line(&self, def_span: Span, params: &[AstParameter], body: &AstStmt) -> usize {
        let after = match params.last() {
            Some(last) => last.span.end().get() as usize,
            None => def_span.begin().get() as usize,
        };
        let before = body.span.begin().get() as usize;
        self.line_of_char(after, before, ')')
            .unwrap_or_else(|| self.start_line(body.span))
    }

    fn raw(&self, span: Span) -> &str {
        self.codemap.source_span(span)
    }

    // --- blank-line rules -------------------------------------------------

    fn skip_line(
        &self,
        top: bool,
        prev: NodeRef,
        cur: &AstStmt,
        cur_line: usize,
        prev_end: usize,
    ) -> bool {
        if prev.is_load() && matches!(cur.node, StmtP::Load(_)) {
            return false;
        }
        if matches!(
            cur.node,
            StmtP::Break | StmtP::Continue | StmtP::Pass | StmtP::Return(_)
        ) {
            return true;
        }
        if (top && self.node_target_like(prev))
            || prev.is_load()
            || prev.is_def()
            || prev.is_if()
            || prev.is_for()
        {
            return true;
        }
        // An assignment / expression statement defers to the blank-line rules
        // of its right-hand-side expression (e.g. a target-like call).
        let inner = match prev {
            NodeRef::Stmt(s) => match &s.node {
                StmtP::Assign(a) => Some(NodeRef::Expr(&a.rhs)),
                StmtP::Expression(x) => Some(NodeRef::Expr(x)),
                _ => None,
            },
            NodeRef::Expr(_) => None,
        };
        if let Some(inner) = inner
            && self.skip_line(top, inner, cur, cur_line, prev_end)
        {
            return true;
        }
        prev_end + 1 != cur_line
    }

    /// A node is target-like for blank-line purposes if it is a target-like
    /// call that carries no inner comment. A trailing comment inside the call
    /// suppresses the forced blank (matching the Go reference, where the
    /// comment wraps the call node and defeats the `*CallExpr` type check).
    fn node_target_like(&self, n: NodeRef) -> bool {
        match n {
            NodeRef::Expr(e) => match &e.node {
                ExprP::Call(_, args) => {
                    is_target_like(&args.args)
                        && !self
                            .comments
                            .any_line_between(self.start_line(e.span), self.end_line(e.span))
                }
                _ => false,
            },
            NodeRef::Stmt(_) => false,
        }
    }
}

/// A reference to either a statement or an expression, for the blank-line
/// predicates (which Go evaluates over `syntax.Node`).
#[derive(Clone, Copy)]
enum NodeRef<'x> {
    Stmt(&'x AstStmt),
    Expr(&'x AstExpr),
}

impl NodeRef<'_> {
    fn is_load(&self) -> bool {
        matches!(self, NodeRef::Stmt(s) if matches!(s.node, StmtP::Load(_)))
    }
    fn is_def(&self) -> bool {
        matches!(self, NodeRef::Stmt(s) if matches!(s.node, StmtP::Def(_)))
    }
    fn is_if(&self) -> bool {
        matches!(self, NodeRef::Stmt(s) if matches!(s.node, StmtP::If(_, _) | StmtP::IfElse(_, _)))
    }
    fn is_for(&self) -> bool {
        matches!(self, NodeRef::Stmt(s) if matches!(s.node, StmtP::For(_)))
    }
}

/// A call is "target-like" (a BUILD target declaration) if it has no positional
/// arguments — i.e. it is empty or every argument is named.
fn is_target_like(args: &[AstArgument]) -> bool {
    args.iter()
        .all(|a| matches!(a.node, ArgumentP::Named(_, _)))
}

/// Statements of a block: the contents of a `Statements` node, or the single
/// statement itself.
fn block_stmts(stmt: &AstStmt) -> &[AstStmt] {
    match &stmt.node {
        StmtP::Statements(v) => v,
        _ => std::slice::from_ref(stmt),
    }
}
