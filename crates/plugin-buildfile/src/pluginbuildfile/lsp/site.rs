//! The site resolver: one classification of the cursor's position, computed
//! from the document's tokens (and, for two name sets, its last good parse),
//! that completion, hover, definition and the `--explain` surface all read.
//!
//! Everything the site kind depends on lies at or before the cursor, so an
//! error after the cursor never changes the answer, and an error before it
//! costs only the bytes of its span. The resolver keeps a bracket stack over
//! the token array, so the enclosing call, its callee and the keyword the
//! cursor sits in are known while the call is unclosed and the file does not
//! parse.
//!
//! **Extents.** A closed bracket's extent ends at its linked closer. An
//! unclosed one ends at the first later line whose first non-blank byte sits
//! at an indent no deeper than the opener's line and is not a closer, skipping
//! blank lines and lines that begin inside a string or comment, or at EOF; at
//! top level that is the next column-0 line. Present keys and the selected
//! driver are read from the extent for every feature, and a guessed extent is
//! reported as `degraded: call_extent_guessed`.
//!
//! **The AST overlay.** When the file parses, two name sets come from the AST
//! that tokens cannot give: the names the file rebinds (a local `def target`
//! disables heph's schema everywhere) and each loaded name with its file. They
//! are name sets, never positions, so the last good parse is at most stale,
//! never misplaced.

use super::lex::{Bracket, ErrorKind, Kind, NO_LINK, Tok, Tokens, line_end, line_start};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// The name sets the last good parse contributes.
#[derive(Clone, Debug, Default)]
pub(crate) struct Overlay {
    /// Every name the file binds (`def`s and parameters, assignments, loop
    /// variables, `load` imports), flat: bound anywhere counts everywhere.
    pub rebound: HashSet<String>,
    /// Each loaded local name → (load path, exported name).
    pub loaded: HashMap<String, (String, String)>,
}

/// Visit `s` and every statement nested within it.
fn walk_stmt<'a>(
    s: &'a starlark::syntax::ast::AstStmt,
    f: &mut impl FnMut(&'a starlark::syntax::ast::AstStmt),
) {
    f(s);
    // `Stmt::visit_stmt` is one level deep; recurse to reach the whole subtree.
    s.visit_stmt(|child| walk_stmt(child, f));
}

/// Every name the module binds: `def`s and their parameters, assignment and loop
/// targets, and `load(...)` imports.
///
/// Deliberately flat — a name bound anywhere counts as bound everywhere, with no
/// scope tracking. Over-approximating errs toward silence, and the alternative
/// (a real scope analysis) would buy nothing: a buffer that defines its own
/// `target` almost certainly means it at every call site.
pub(crate) fn bound_names(ast: &starlark::syntax::AstModule) -> HashSet<&str> {
    use starlark::syntax::ast::Stmt;
    let mut names = HashSet::new();
    walk_stmt(ast.statement(), &mut |s| match &s.node {
        Stmt::Def(def) => {
            names.insert(def.name.node.ident.as_str());
            names.extend(
                def.params
                    .iter()
                    .filter_map(|p| p.ident())
                    .map(|i| i.node.ident.as_str()),
            );
        }
        Stmt::Load(load) => {
            names.extend(load.args.iter().map(|a| a.local.node.ident.as_str()));
        }
        Stmt::Assign(assign) => collect_assign_target(&assign.lhs, &mut names),
        Stmt::AssignModify(target, _, _) => collect_assign_target(target, &mut names),
        Stmt::For(for_stmt) => collect_assign_target(&for_stmt.var, &mut names),
        _ => {}
    });
    names
}

/// Add the identifiers an assignment or loop target binds (`a`, `a, b`, `[a, b]`).
/// Index and attribute targets (`a[0] = …`, `a.b = …`) mutate an existing
/// binding rather than introducing one, so they bind nothing.
fn collect_assign_target<'a>(
    target: &'a starlark::syntax::ast::AstAssignTarget,
    names: &mut HashSet<&'a str>,
) {
    use starlark::syntax::ast::AssignTarget;
    match &target.node {
        AssignTarget::Identifier(id) => {
            names.insert(id.node.ident.as_str());
        }
        AssignTarget::Tuple(items) => {
            for item in items {
                collect_assign_target(item, names);
            }
        }
        AssignTarget::Index(_) | AssignTarget::Dot(_, _) => {}
    }
}

/// The overlay of a parsed module: its bound names and its `load` imports.
pub(crate) fn overlay_of(ast: &starlark::syntax::AstModule) -> Overlay {
    use starlark::syntax::ast::Stmt;
    let rebound = bound_names(ast).into_iter().map(str::to_string).collect();
    let mut loaded = HashMap::new();
    let mut add = |s: &starlark::syntax::ast::AstStmt| {
        if let Stmt::Load(load) = &s.node {
            for arg in &load.args {
                loaded.insert(
                    arg.local.node.ident.clone(),
                    (load.module.node.clone(), arg.their.node.clone()),
                );
            }
        }
    };
    add(ast.statement());
    ast.statement().visit_stmt(&mut add);
    Overlay { rebound, loaded }
}

/// Whether the file rebinds `name` (`def target(...)`, `target = …`, a
/// `load` of it): heph's schema for that builtin does not apply. Shared by the
/// resolver and the diagnostics, so the two never disagree.
pub(crate) fn is_rebound(overlay: Option<&Overlay>, name: &str) -> bool {
    overlay.is_some_and(|o| o.rebound.contains(name))
}

/// How far the overlay is from the current text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AstState {
    /// The current text parsed.
    Current,
    /// The current text does not parse; the overlay is from a version this
    /// many edits behind.
    Stale { behind: i32 },
    /// No version of the document has parsed yet.
    Absent,
}

/// A heph builtin call the resolver recognises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Target,
    ProviderState,
    Load,
}

impl Builtin {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Builtin::Target => "target",
            Builtin::ProviderState => "provider_state",
            Builtin::Load => "load",
        }
    }

    fn of(name: &str) -> Option<Builtin> {
        match name {
            "target" => Some(Builtin::Target),
            "provider_state" => Some(Builtin::ProviderState),
            _ => None,
        }
    }
}

/// The enclosing (or hovered) builtin call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Call {
    pub callee: Builtin,
    pub callee_span: Range<usize>,
    /// Byte offset of the `(`.
    pub open: usize,
    /// Where the extent ends (exclusive).
    pub end: usize,
    /// `Some(line)` when the extent was guessed; the 0-based line it stopped
    /// at.
    pub guessed: Option<u32>,
    /// The keyword arguments present in the extent (the one under the cursor
    /// excluded).
    pub keys: Vec<String>,
    /// The `driver = "…"` (for `target`) or `provider = "…"` (for
    /// `provider_state`) string literal in the extent, if any.
    pub selected: Option<String>,
}

/// How the value being completed is quoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Quote {
    /// No string yet: `driver = │` or `driver = ex│`.
    Absent,
    /// An unterminated string: `driver = "ex│`.
    Open,
    /// A closed string: `driver = "ex│"`.
    Closed,
}

impl Quote {
    fn name(self) -> &'static str {
        match self {
            Quote::Absent => "absent",
            Quote::Open => "open",
            Quote::Closed => "closed",
        }
    }
}

/// A string (or not-yet-string) value at the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Str {
    /// The bytes a completion replaces: the content of a closed string; for an
    /// unterminated one, the opening quote (and prefix letter) through the run
    /// of characters valid for the site; for no string yet, the identifier
    /// typed so far (or nothing).
    pub edit: Range<usize>,
    /// The whole token: the closed string with its quotes, the error span of
    /// an unterminated one, or the identifier.
    pub token: Range<usize>,
    /// The content before the cursor.
    pub prefix: String,
    /// The whole content (closed strings: between the quotes; unterminated:
    /// the valid run).
    pub content: String,
    pub quote: Quote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    None,
    /// The `target` / `provider_state` identifier of a call.
    Callee {
        call: Call,
    },
    DriverValue {
        call: Call,
        value: Str,
    },
    ProviderValue {
        call: Call,
        value: Str,
    },
    /// A keyword-name position inside `target(`.
    TargetKey {
        call: Call,
        edit: Range<usize>,
        prefix: String,
    },
    /// A keyword-name position inside `provider_state(`.
    StateKey {
        call: Call,
        edit: Range<usize>,
        prefix: String,
    },
    /// After `heph.`.
    PluginNamespace {
        edit: Range<usize>,
        prefix: String,
    },
    /// After `heph.<plugin>.`, or on the function name.
    PluginMember {
        plugin: String,
        edit: Range<usize>,
        prefix: String,
    },
    /// A `//pkg:name` or `:name` string.
    Address {
        value: Str,
    },
    /// The first argument of `load(`.
    LoadPath {
        value: Str,
    },
    /// A later string argument of `load(`.
    LoadSymbol {
        path: String,
        value: Str,
    },
}

impl Site {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Site::None => "none",
            Site::Callee { .. } => "callee",
            Site::DriverValue { .. } => "driver_value",
            Site::ProviderValue { .. } => "provider_value",
            Site::TargetKey { .. } => "target_key",
            Site::StateKey { .. } => "state_key",
            Site::PluginNamespace { .. } => "plugin_namespace",
            Site::PluginMember { .. } => "plugin_member",
            Site::Address { .. } => "address",
            Site::LoadPath { .. } => "load_path",
            Site::LoadSymbol { .. } => "load_symbol",
        }
    }

    pub(crate) fn call(&self) -> Option<&Call> {
        match self {
            Site::Callee { call }
            | Site::DriverValue { call, .. }
            | Site::ProviderValue { call, .. }
            | Site::TargetKey { call, .. }
            | Site::StateKey { call, .. } => Some(call),
            _ => None,
        }
    }

    /// The keyword argument the cursor sits in the value of.
    pub(crate) fn keyword(&self) -> Option<&'static str> {
        match self {
            Site::DriverValue { .. } => Some("driver"),
            Site::ProviderValue { .. } => Some("provider"),
            _ => None,
        }
    }

    /// The text before the cursor that completion filters by.
    pub(crate) fn prefix(&self) -> Option<&str> {
        match self {
            Site::None | Site::Callee { .. } => None,
            Site::DriverValue { value, .. }
            | Site::ProviderValue { value, .. }
            | Site::Address { value }
            | Site::LoadPath { value }
            | Site::LoadSymbol { value, .. } => Some(&value.prefix),
            Site::TargetKey { prefix, .. }
            | Site::StateKey { prefix, .. }
            | Site::PluginNamespace { prefix, .. }
            | Site::PluginMember { prefix, .. } => Some(prefix),
        }
    }

    pub(crate) fn value(&self) -> Option<&Str> {
        match self {
            Site::DriverValue { value, .. }
            | Site::ProviderValue { value, .. }
            | Site::Address { value }
            | Site::LoadPath { value }
            | Site::LoadSymbol { value, .. } => Some(value),
            _ => None,
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        let span = |r: &Range<usize>| json!([r.start, r.end]);
        let mut out = json!({
            "kind": self.kind(),
            "call": self.call().map(|c| c.callee.name()),
            "keyword": self.keyword(),
            "prefix": self.prefix(),
        });
        if let (Some(call), Some(map)) = (self.call(), out.as_object_mut()) {
            map.insert("present_keys".to_string(), json!(call.keys));
            map.insert("selected".to_string(), json!(call.selected));
        }
        if let (Some(value), Some(map)) = (self.value(), out.as_object_mut()) {
            map.insert("quote".to_string(), json!(value.quote.name()));
            map.insert("edit".to_string(), span(&value.edit));
            map.insert("token".to_string(), span(&value.token));
        }
        let word = match self {
            Site::TargetKey { edit, .. }
            | Site::StateKey { edit, .. }
            | Site::PluginNamespace { edit, .. }
            | Site::PluginMember { edit, .. } => Some(edit),
            _ => None,
        };
        if let (Some(edit), Some(map)) = (word, out.as_object_mut()) {
            map.insert("edit".to_string(), span(edit));
            map.insert("token".to_string(), span(edit));
        }
        if let (Site::PluginMember { plugin, .. }, Some(map)) = (self, out.as_object_mut()) {
            map.insert("plugin".to_string(), json!(plugin));
        }
        out
    }
}

/// Why a heph answer is empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The file rebinds the callee (`def target(...)`), so heph's schema does
    /// not apply.
    CalleeRebound {
        name: String,
    },
    /// The cursor sits inside a lexer error's span; `span`, `line` and
    /// `message` name the root cause (the first error on that line).
    InLexError {
        kind: &'static str,
        span: Range<usize>,
        line: u32,
        message: String,
    },
    NotASite,
    /// The registry snapshot is still loading.
    ProvidersLoading,
    /// The document lies outside the repo the server is bound to.
    OutsideRoot,
}

impl Reason {
    fn to_json(&self) -> Value {
        match self {
            Reason::CalleeRebound { name } => json!({"reason": "callee_rebound", "name": name}),
            Reason::InLexError {
                kind,
                span,
                line,
                message,
            } => json!({
                "reason": "in_lex_error",
                "kind": kind,
                "span": [span.start, span.end],
                "line": line,
                "message": message,
            }),
            Reason::NotASite => json!({"reason": "not_a_site"}),
            Reason::ProvidersLoading => json!({"reason": "providers_loading"}),
            Reason::OutsideRoot => json!({"reason": "outside_root"}),
        }
    }
}

/// What a present answer could not do in full.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Degraded {
    /// The enclosing call is unclosed; its extent stopped at this line.
    CallExtentGuessed { line: u32 },
    /// The value is an unterminated string: the edit stops at the end of the
    /// run of characters valid for the site.
    EditToWordEnd,
    /// The name sets come from a parse this many versions behind.
    AstStale { behind: i32 },
    /// No version of the document has parsed: no name sets at all.
    AstAbsent,
    /// A lexer error lies before the cursor; its span costs only its bytes.
    LexErrorBefore { span: Range<usize> },
}

impl Degraded {
    fn to_json(&self) -> Value {
        match self {
            Degraded::CallExtentGuessed { line } => {
                json!({"degraded": "call_extent_guessed", "line": line})
            }
            Degraded::EditToWordEnd => json!({"degraded": "edit_to_word_end"}),
            Degraded::AstStale { behind } => json!({"degraded": "ast_stale", "behind": behind}),
            Degraded::AstAbsent => json!({"degraded": "ast_absent"}),
            Degraded::LexErrorBefore { span } => {
                json!({"degraded": "lex_error_before", "span": [span.start, span.end]})
            }
        }
    }
}

/// The resolver's answer: the site, why it is empty when it is, and what a
/// present answer could not do in full.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolution {
    pub site: Site,
    pub reason: Option<Reason>,
    pub degraded: Vec<Degraded>,
}

impl Resolution {
    pub(crate) fn none(reason: Reason) -> Resolution {
        Resolution {
            site: Site::None,
            reason: Some(reason),
            degraded: Vec::new(),
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "site": self.site.to_json(),
            "reason": self.reason.as_ref().map(Reason::to_json),
            "degraded": self.degraded.iter().map(Degraded::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Everything the resolver reads about one document.
pub(crate) struct Doc<'a> {
    pub text: &'a str,
    pub tokens: &'a Tokens,
    pub overlay: Option<&'a Overlay>,
    pub ast: AstState,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Characters valid in a driver or provider name, for the end of an
/// unterminated string's edit.
fn is_name_byte(b: u8) -> bool {
    is_ident_byte(b) || b == b'-' || b == b'.'
}

/// Characters valid in an address or load path.
fn is_address_byte(b: u8) -> bool {
    is_name_byte(b) || matches!(b, b'/' | b':' | b'@' | b'+' | b'~')
}

impl Doc<'_> {
    fn toks(&self) -> &[Tok] {
        &self.tokens.toks
    }

    fn tok(&self, i: usize) -> Option<&Tok> {
        self.tokens.toks.get(i)
    }

    fn text_of(&self, t: &Tok) -> &str {
        self.text.get(t.range()).unwrap_or("")
    }

    /// Index of the first token starting at or after `o`.
    fn first_at_or_after(&self, o: usize) -> usize {
        self.toks().partition_point(|t| (t.start as usize) < o)
    }

    /// The last significant (non-comment) token ending at or before `o`,
    /// at an index above `floor`.
    fn prev_significant(&self, o: usize, floor: Option<usize>) -> Option<usize> {
        let mut i = self.toks().partition_point(|t| (t.end as usize) <= o);
        while i > 0 {
            i -= 1;
            if floor.is_some_and(|f| i <= f) {
                return None;
            }
            if self.tok(i).is_some_and(|t| t.kind != Kind::Comment) {
                return Some(i);
            }
        }
        None
    }

    /// Whether `pos` lies strictly inside a string-like token that started
    /// before it: a closed string or an unterminated one.
    fn inside_string(&self, pos: usize) -> bool {
        let i = self.toks().partition_point(|t| (t.start as usize) < pos);
        i > 0
            && self.tok(i - 1).is_some_and(|t| {
                matches!(
                    t.kind,
                    Kind::Str | Kind::Error(ErrorKind::UnterminatedString { .. })
                ) && (t.end as usize) > pos
            })
    }

    /// The indent (bytes of leading blanks) of the line containing `at`.
    fn indent_of(&self, at: usize) -> usize {
        let ls = line_start(self.text, at);
        self.text.get(ls..).map_or(0, |rest| {
            rest.len() - rest.trim_start_matches([' ', '\t']).len()
        })
    }

    fn line_of(&self, at: usize) -> u32 {
        self.text
            .get(..at.min(self.text.len()))
            .map_or(0, |b| b.matches('\n').count() as u32)
    }

    /// The guessed extent of the unclosed opener at `open`: the start of the
    /// first later line that ends it, and that line's number (or EOF).
    fn guessed_extent(&self, open: usize) -> (usize, u32) {
        let indent = self.indent_of(open);
        let mut ls = line_end(self.text, open) + 1;
        while ls < self.text.len() {
            let le = line_end(self.text, ls);
            let line = self.text.get(ls..le).unwrap_or("");
            if let Some(i) = line.find(|c: char| c != ' ' && c != '\t' && c != '\r') {
                let first = line.as_bytes().get(i).copied().unwrap_or(b' ');
                let skipped = first == b'#' || self.inside_string(ls) || self.inside_string(ls + i);
                if !skipped && i <= indent && !matches!(first, b')' | b']' | b'}') {
                    return (ls, self.line_of(ls));
                }
            }
            ls = le + 1;
        }
        // Runs to EOF, the end of the text included (a cursor there is still
        // inside the call).
        (self.text.len() + 1, self.line_of(self.text.len()))
    }

    /// The end of opener `i`'s extent, and the guessed line when it was not
    /// closed by a matching closer.
    fn extent(&self, i: usize) -> (usize, Option<u32>) {
        let Some(t) = self.tok(i) else {
            return (self.text.len(), None);
        };
        if t.link == NO_LINK {
            let (end, line) = self.guessed_extent(t.start as usize);
            return (end, Some(line));
        }
        let closer = self
            .tok(t.link as usize)
            .map_or(self.text.len(), |c| c.start as usize);
        if t.popped {
            (closer, Some(self.line_of(closer)))
        } else {
            (closer, None)
        }
    }

    /// The openers still open at `o`, outermost first.
    fn open_stack(&self, o: usize) -> Vec<usize> {
        let mut stack: Vec<usize> = Vec::new();
        let upto = self.first_at_or_after(o);
        for i in 0..upto {
            let Some(t) = self.tok(i) else { continue };
            match t.kind {
                Kind::Open(_) => stack.push(i),
                Kind::Close(_) if t.link != NO_LINK => {
                    // Pop the openers this closer ended (its match and any
                    // mismatched ones above it).
                    while let Some(&top) = stack.last() {
                        if self.tok(top).is_some_and(|x| x.link as usize == i) {
                            stack.pop();
                        } else {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        // An unclosed opener whose guessed extent ended before the cursor is
        // no longer open.
        stack.retain(|&i| {
            self.tok(i)
                .is_none_or(|t| t.link != NO_LINK || self.guessed_extent(t.start as usize).0 > o)
        });
        stack
    }

    /// The builtin callee of the round opener at `i`, with its span.
    fn callee_of(&self, i: usize) -> Option<(Builtin, Range<usize>)> {
        let t = self.tok(i)?;
        if t.kind != Kind::Open(Bracket::Round) || i == 0 {
            return None;
        }
        let c = self.tok(i - 1)?;
        if i >= 2 && self.tok(i - 2).is_some_and(|d| d.kind == Kind::Dot) {
            return None;
        }
        let builtin = match c.kind {
            Kind::Load => Builtin::Load,
            Kind::Ident => Builtin::of(self.text_of(c))?,
            _ => return None,
        };
        Some((builtin, c.range()))
    }

    /// The closed-string content of token `t` (between its quotes).
    fn string_content(&self, t: &Tok) -> Option<Range<usize>> {
        let text = self.text_of(t);
        let body_at = text.find(['"', '\''])?;
        let quote = text.as_bytes().get(body_at).copied()?;
        let triple = text
            .get(body_at..)
            .is_some_and(|b| b.starts_with(&(quote as char).to_string().repeat(3)));
        let q = if triple { 3 } else { 1 };
        let start = t.start as usize + body_at + q;
        let end = (t.end as usize).checked_sub(q)?;
        (start <= end).then_some(start..end)
    }

    /// The call whose `(` is token `i`.
    fn call_at(&self, i: usize, cursor: Option<usize>) -> Option<Call> {
        let (callee, callee_span) = self.callee_of(i)?;
        let open = self.tok(i)?.start as usize;
        let (end, guessed) = self.extent(i);
        let mut keys = Vec::new();
        let mut selected = None;
        let selector = match callee {
            Builtin::Target => "driver",
            Builtin::ProviderState => "provider",
            Builtin::Load => "",
        };
        let mut depth = 0usize;
        let mut j = i + 1;
        while let Some(t) = self.tok(j) {
            if t.start as usize >= end {
                break;
            }
            match t.kind {
                Kind::Open(_) => depth += 1,
                Kind::Close(_) => depth = depth.saturating_sub(1),
                Kind::Ident if depth == 0 => {
                    let next = self.tok(j + 1);
                    if next.is_some_and(|n| n.kind == Kind::Equal) {
                        let name = self.text_of(t);
                        let under =
                            cursor.is_some_and(|o| t.start as usize <= o && o <= t.end as usize);
                        if !under && !keys.iter().any(|k| k == name) {
                            keys.push(name.to_string());
                        }
                        if name == selector
                            && let Some(v) = self.tok(j + 2)
                            && v.kind == Kind::Str
                        {
                            selected = self
                                .string_content(v)
                                .and_then(|r| self.text.get(r))
                                .map(str::to_string);
                        }
                    }
                }
                _ => {}
            }
            j += 1;
        }
        Some(Call {
            callee,
            callee_span,
            open,
            end,
            guessed,
            keys,
            selected,
        })
    }

    /// The identifier-like token under the cursor (the cursor anywhere in it,
    /// or right after it).
    fn word_at(&self, o: usize) -> Option<usize> {
        let i = self.toks().partition_point(|t| (t.end as usize) < o);
        let t = self.tok(i)?;
        let word = matches!(t.kind, Kind::Ident | Kind::Load | Kind::Keyword);
        (word && t.start as usize <= o && o <= t.end as usize).then_some(i)
    }

    /// The token whose span contains the cursor by its kind's boundary rule.
    fn inside(&self, o: usize) -> Option<usize> {
        let i = self.toks().partition_point(|t| (t.start as usize) < o);
        // Only a token starting before the cursor can contain it.
        (0..i)
            .rev()
            .take(2)
            .find(|&j| self.tok(j).is_some_and(|t| t.contains(o)))
    }

    /// A string value at token `i`, closed or unterminated.
    fn str_at(&self, i: usize, o: usize, valid: fn(u8) -> bool) -> Option<Str> {
        let t = self.tok(i)?;
        match t.kind {
            Kind::Str => {
                let content = self.string_content(t)?;
                let prefix = self
                    .text
                    .get(content.start..o.max(content.start))?
                    .to_string();
                Some(Str {
                    edit: content.clone(),
                    token: t.range(),
                    content: self.text.get(content.clone())?.to_string(),
                    prefix,
                    quote: Quote::Closed,
                })
            }
            Kind::Error(ErrorKind::UnterminatedString { triple: false }) => {
                let text = self.text_of(t);
                let quote_at = text.find(['"', '\''])?;
                let content_start = t.start as usize + quote_at + 1;
                let bytes = self.text.as_bytes();
                let mut run_end = content_start;
                while run_end < t.end as usize && bytes.get(run_end).copied().is_some_and(valid) {
                    run_end += 1;
                }
                let prefix_end = o.clamp(content_start, t.end as usize);
                Some(Str {
                    edit: t.start as usize..run_end.max(o.min(t.end as usize)).min(t.end as usize),
                    token: t.range(),
                    content: self.text.get(content_start..run_end)?.to_string(),
                    prefix: self.text.get(content_start..prefix_end)?.to_string(),
                    quote: Quote::Open,
                })
            }
            _ => None,
        }
    }

    /// The root cause of the lexer error at token `i`: the first error on its
    /// line.
    fn lex_error_reason(&self, i: usize) -> Reason {
        let t = self.tok(i).copied();
        let line_start_at = t.map_or(0, |t| line_start(self.text, t.start as usize));
        let root = self
            .tokens
            .errors
            .iter()
            .filter_map(|e| self.tok(e.token as usize).map(|t| (e, t)))
            .find(|(_, t)| line_start(self.text, t.start as usize) == line_start_at)
            .map(|(e, t)| (e.message.clone(), *t))
            .or_else(|| t.map(|t| (self.tokens.error_message(i).unwrap_or("").to_string(), t)));
        match root {
            Some((message, t)) => Reason::InLexError {
                kind: match t.kind {
                    Kind::Error(ErrorKind::UnterminatedString { .. }) => "unterminated_string",
                    _ => "other",
                },
                span: t.range(),
                line: self.line_of(t.start as usize),
                message,
            },
            None => Reason::NotASite,
        }
    }

    /// The nearest lexer error ending at or before `o` that does not contain it.
    fn error_before(&self, o: usize) -> Option<Range<usize>> {
        let upto = self.first_at_or_after(o);
        self.toks()
            .get(..upto)?
            .iter()
            .rev()
            .find(|t| matches!(t.kind, Kind::Error(_)) && !t.contains(o) && (t.end as usize) <= o)
            .map(Tok::range)
    }
}

/// Classify byte offset `o` of `doc`. `None` positions (past the end) are
/// handled by the caller.
pub(crate) fn resolve(doc: &Doc<'_>, o: usize) -> Resolution {
    let site = classify(doc, o);
    let mut resolution = match site {
        Ok(site) => Resolution {
            site,
            reason: None,
            degraded: Vec::new(),
        },
        Err(reason) => return Resolution::none(reason),
    };
    // Shadowing, decided here for every feature.
    let callee = match &resolution.site {
        Site::Callee { call }
        | Site::DriverValue { call, .. }
        | Site::ProviderValue { call, .. }
        | Site::TargetKey { call, .. }
        | Site::StateKey { call, .. } => Some(call.callee.name()),
        Site::PluginNamespace { .. } | Site::PluginMember { .. } => Some("heph"),
        _ => None,
    };
    let ast_degraded = match doc.ast {
        AstState::Current => None,
        AstState::Stale { behind } => Some(Degraded::AstStale { behind }),
        AstState::Absent => Some(Degraded::AstAbsent),
    };
    if let Some(name) = callee
        && is_rebound(doc.overlay, name)
    {
        // The rebinding came from the overlay, so its staleness is part of the
        // answer even though the answer is empty.
        return Resolution {
            site: Site::None,
            reason: Some(Reason::CalleeRebound {
                name: name.to_string(),
            }),
            degraded: ast_degraded.into_iter().collect(),
        };
    }
    if let Some(call) = resolution.site.call()
        && let Some(line) = call.guessed
    {
        resolution
            .degraded
            .push(Degraded::CallExtentGuessed { line });
    }
    if resolution
        .site
        .value()
        .is_some_and(|v| v.quote == Quote::Open)
    {
        resolution.degraded.push(Degraded::EditToWordEnd);
    }
    resolution.degraded.extend(ast_degraded);
    if let Some(span) = doc.error_before(o) {
        resolution.degraded.push(Degraded::LexErrorBefore { span });
    }
    resolution
}

fn classify(doc: &Doc<'_>, o: usize) -> Result<Site, Reason> {
    let inside = doc.inside(o);
    let inside_tok = inside.and_then(|i| doc.tok(i)).copied();

    // A comment is never a site.
    if inside_tok.is_some_and(|t| t.kind == Kind::Comment) {
        return Err(Reason::NotASite);
    }

    let stack = doc.open_stack(o);
    let innermost = stack.last().copied();
    let enclosing_call = innermost.and_then(|i| doc.call_at(i, Some(o)).map(|c| (i, c)));

    // A string at the cursor: closed, or unterminated (not a triple quote).
    if let Some(i) = inside
        && let Some(t) = inside_tok
        && matches!(
            t.kind,
            Kind::Str | Kind::Error(ErrorKind::UnterminatedString { triple: false })
        )
    {
        return string_site(doc, i, o, enclosing_call).ok_or_else(|| {
            if matches!(t.kind, Kind::Error(_)) {
                doc.lex_error_reason(i)
            } else {
                Reason::NotASite
            }
        });
    }
    // Any other lexer error at the cursor.
    if let Some(i) = inside
        && inside_tok.is_some_and(|t| matches!(t.kind, Kind::Error(_)))
    {
        return Err(doc.lex_error_reason(i));
    }
    if inside_tok.is_some_and(|t| t.kind == Kind::Str) {
        return Err(Reason::NotASite);
    }

    let word = doc.word_at(o);
    let word_tok = word.and_then(|i| doc.tok(i)).copied();
    let word_start = word_tok.map_or(o, |t| t.start as usize);
    let edit = word_tok.map_or(o..o, |t| t.range());
    let prefix = doc.text.get(word_start..o).unwrap_or("").to_string();

    // The callee identifier of a builtin call.
    if let (Some(i), Some(t)) = (word, word_tok)
        && t.kind == Kind::Ident
        && Builtin::of(doc.text_of(&t)).is_some()
        && doc
            .tok(i + 1)
            .is_some_and(|n| n.kind == Kind::Open(Bracket::Round))
        && let Some(call) = doc.call_at(i + 1, None)
    {
        return Ok(Site::Callee { call });
    }

    // `heph.` and `heph.<plugin>.` member paths.
    if let Some(site) = dotted_site(doc, word, o, &edit, &prefix) {
        return Ok(site);
    }

    let Some((open_idx, call)) = enclosing_call else {
        return Err(Reason::NotASite);
    };
    if call.callee == Builtin::Load {
        return Err(Reason::NotASite);
    }
    let prev = doc.prev_significant(word_start, Some(open_idx));
    let prev_tok = prev.and_then(|i| doc.tok(i)).copied();
    match prev_tok.map(|t| t.kind) {
        // Right after `(` or `,`: a key.
        None | Some(Kind::Comma) => Ok(key_site(call, edit, prefix)),
        Some(Kind::Equal) => {
            // A value position: only `driver =` / `provider =` is a site (D9).
            let keyword = prev
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| doc.tok(i))
                .filter(|k| k.kind == Kind::Ident)
                .map(|k| doc.text_of(k).to_string());
            let value = Str {
                edit: edit.clone(),
                token: edit,
                content: prefix.clone(),
                prefix,
                quote: Quote::Absent,
            };
            match (call.callee, keyword.as_deref()) {
                (Builtin::Target, Some("driver")) => Ok(Site::DriverValue { call, value }),
                (Builtin::ProviderState, Some("provider")) => {
                    Ok(Site::ProviderValue { call, value })
                }
                _ => Err(Reason::NotASite),
            }
        }
        // After a complete argument with no comma: still a key; the bracket
        // stack needs no commas.
        Some(Kind::Str | Kind::Ident | Kind::Number | Kind::Close(_) | Kind::Error(_))
            if word_tok.is_none_or(|t| t.kind == Kind::Ident) =>
        {
            Ok(key_site(call, edit, prefix))
        }
        _ => Err(Reason::NotASite),
    }
}

fn key_site(call: Call, edit: Range<usize>, prefix: String) -> Site {
    match call.callee {
        Builtin::ProviderState => Site::StateKey { call, edit, prefix },
        _ => Site::TargetKey { call, edit, prefix },
    }
}

fn dotted_site(
    doc: &Doc<'_>,
    word: Option<usize>,
    o: usize,
    edit: &Range<usize>,
    prefix: &str,
) -> Option<Site> {
    // The token before the member being typed (or the cursor) must be a `.`.
    let member_start = word
        .and_then(|i| doc.tok(i))
        .map_or(o, |t| t.start as usize);
    let dot = doc.prev_significant(member_start, None)?;
    let dot_tok = doc.tok(dot)?;
    if dot_tok.kind != Kind::Dot || dot_tok.end as usize != member_start {
        return None;
    }
    // Walk left over `ident .` pairs.
    let mut segments: Vec<&str> = Vec::new();
    let mut i = dot;
    loop {
        let name = i.checked_sub(1).and_then(|j| doc.tok(j))?;
        if name.kind != Kind::Ident {
            return None;
        }
        segments.push(doc.text_of(name));
        let before = i.checked_sub(2).and_then(|j| doc.tok(j));
        match before {
            Some(b) if b.kind == Kind::Dot && b.end == name.start => i -= 2,
            _ => break,
        }
    }
    segments.reverse();
    match segments.as_slice() {
        ["heph"] => Some(Site::PluginNamespace {
            edit: edit.clone(),
            prefix: prefix.to_string(),
        }),
        ["heph", plugin] => Some(Site::PluginMember {
            plugin: (*plugin).to_string(),
            edit: edit.clone(),
            prefix: prefix.to_string(),
        }),
        _ => None,
    }
}

fn string_site(
    doc: &Doc<'_>,
    i: usize,
    o: usize,
    enclosing: Option<(usize, Call)>,
) -> Option<Site> {
    let t = *doc.tok(i)?;
    let prev = doc.prev_significant(t.start as usize, None);
    let prev_tok = prev.and_then(|p| doc.tok(p)).copied();
    let keyword = match prev_tok {
        Some(p) if p.kind == Kind::Equal => prev
            .and_then(|p| p.checked_sub(1))
            .and_then(|k| doc.tok(k))
            .filter(|k| k.kind == Kind::Ident)
            .map(|k| doc.text_of(k).to_string()),
        _ => None,
    };
    if let Some((open_idx, call)) = enclosing {
        // Directly inside the call (the innermost bracket is its `(`).
        let direct = prev.is_some_and(|p| p > open_idx);
        match (call.callee, keyword.as_deref()) {
            (Builtin::Target, Some("driver")) if direct => {
                let value = doc.str_at(i, o, is_name_byte)?;
                return Some(Site::DriverValue { call, value });
            }
            (Builtin::ProviderState, Some("provider")) if direct => {
                let value = doc.str_at(i, o, is_name_byte)?;
                return Some(Site::ProviderValue { call, value });
            }
            (Builtin::Load, _) => {
                let value = doc.str_at(i, o, is_address_byte)?;
                if prev == Some(open_idx) {
                    return Some(Site::LoadPath { value });
                }
                let path = doc
                    .tok(open_idx + 1)
                    .filter(|p| p.kind == Kind::Str)
                    .and_then(|p| doc.string_content(p))
                    .and_then(|r| doc.text.get(r))?
                    .to_string();
                return Some(Site::LoadSymbol { path, value });
            }
            _ => {}
        }
    }
    let value = doc.str_at(i, o, is_address_byte)?;
    (value.prefix.starts_with("//") || value.prefix.starts_with(':'))
        .then_some(Site::Address { value })
}

/// The selected driver (`target`) or provider (`provider_state`) of the call
/// whose `(` is the first bracket at or after byte `from` — the callee's end.
/// The one reading of `driver = "…"` that completion, hover and diagnostics
/// share.
pub(crate) fn selected_after(doc: &Doc<'_>, from: usize) -> Option<String> {
    let i = doc.first_at_or_after(from);
    let t = doc.tok(i)?;
    if t.kind != Kind::Open(Bracket::Round) {
        return None;
    }
    doc.call_at(i, None)?.selected
}

/// The `:` trigger's light address check (D7): the cursor is inside a string
/// literal whose text before the cursor has address shape — a leading `:`,
/// `//pkg:`, or a package path followed by `:`.
pub(crate) fn colon_is_address(doc: &Doc<'_>, o: usize) -> bool {
    let Some(i) = doc.inside(o) else {
        return false;
    };
    let Some(value) = doc.str_at(i, o, is_address_byte) else {
        return false;
    };
    let p = value.prefix.as_str();
    if p.starts_with(':') || p.starts_with("//") {
        return p.contains(':');
    }
    // A package path followed by `:`: `pkg/sub:`.
    p.split_once(':').is_some_and(|(pkg, _)| {
        !pkg.is_empty() && pkg.bytes().all(|b| is_name_byte(b) || b == b'/')
    })
}

/// The `/` trigger fires on a site only inside a string.
pub(crate) fn slash_in_string(doc: &Doc<'_>, o: usize) -> bool {
    doc.inside(o).and_then(|i| doc.tok(i)).is_some_and(|t| {
        matches!(
            t.kind,
            Kind::Str | Kind::Error(ErrorKind::UnterminatedString { .. })
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::lex::lex;
    use super::super::position::{self, Encoding};
    use super::*;
    use lsp_types::Position;

    /// Resolve the `│` in `fixture`.
    fn at(fixture: &str) -> Resolution {
        let (text, o) = split(fixture);
        let tokens = lex(&text);
        resolve(
            &Doc {
                text: &text,
                tokens: &tokens,
                overlay: None,
                ast: AstState::Current,
            },
            o,
        )
    }

    fn split(fixture: &str) -> (String, usize) {
        let o = fixture.find('│').expect("cursor marker");
        (fixture.replacen('│', "", 1), o)
    }

    fn with_overlay(fixture: &str, rebound: &[&str]) -> Resolution {
        let (text, o) = split(fixture);
        let tokens = lex(&text);
        let overlay = Overlay {
            rebound: rebound.iter().map(|s| s.to_string()).collect(),
            loaded: HashMap::new(),
        };
        resolve(
            &Doc {
                text: &text,
                tokens: &tokens,
                overlay: Some(&overlay),
                ast: AstState::Current,
            },
            o,
        )
    }

    /// Resolve an LSP position (line, UTF-16 column) the way the server does.
    fn at_position(text: &str, line: u32, character: u32) -> Option<Resolution> {
        let o = position::to_offset(text, Position::new(line, character), Encoding::Utf16)?;
        let tokens = lex(text);
        Some(resolve(
            &Doc {
                text,
                tokens: &tokens,
                overlay: None,
                ast: AstState::Current,
            },
            o,
        ))
    }

    fn kind(r: &Resolution) -> &'static str {
        r.site.kind()
    }

    #[test]
    fn site_position_past_the_end_of_the_line_or_document() {
        let text = "target(dri\nx = 1\n";
        assert!(at_position(text, 0, 11).is_none(), "past the line end");
        assert!(at_position(text, 9, 0).is_none(), "past the last line");
        let r = at_position(text, 0, 10).expect("end of line is a position");
        assert_eq!(kind(&r), "target_key");
    }

    #[test]
    fn site_non_ascii_before_the_cursor() {
        let text = "target(name = \"café\", dri)\n";
        // `dri` ends at UTF-16 column 25 (the `é` is one unit, two bytes).
        let r = at_position(text, 0, 25).expect("in range");
        assert_eq!(kind(&r), "target_key");
        assert_eq!(r.site.prefix(), Some("dri"));
    }

    #[test]
    fn site_comment_or_non_address_string() {
        assert_eq!(kind(&at("target(name = \"x\") # dri│\n")), "none");
        assert_eq!(kind(&at("target(name = \"hel│lo\")\n")), "none");
        assert_eq!(at("x = \"plain│\"\n").reason, Some(Reason::NotASite));
    }

    #[test]
    fn site_single_quoted_driver_value() {
        let single = at("target(name = 'x', driver = 'ex│')\n");
        let double = at("target(name = \"x\", driver = \"ex│\")\n");
        assert_eq!(kind(&single), "driver_value");
        assert_eq!(single.site.prefix(), double.site.prefix());
        assert_eq!(single.site.keyword(), Some("driver"));
    }

    #[test]
    fn site_nested_calls() {
        let r = at("target(name = f(x, dri│\n");
        assert_eq!(kind(&r), "none", "the inner call is the site, not target");
    }

    #[test]
    fn site_value_position() {
        assert_eq!(kind(&at("target(name = \"x\", cmd = M│)\n")), "none");
    }

    #[test]
    fn site_key_already_present_twice() {
        let r = at("target(name = \"a\", name = \"b\", │)\n");
        let call = r.site.call().expect("a call");
        assert_eq!(call.keys, vec!["name".to_string()], "excluded once");
    }

    #[test]
    fn site_triple_quoted_string_then_cursor() {
        let r = at("x = \"\"\"doc\n(not a call\n\"\"\"\ntarget(dri│\n");
        assert_eq!(kind(&r), "target_key", "the bracket stack is unaffected");
    }

    #[test]
    fn site_target_rebound_by_def_and_by_assignment() {
        // The overlay carries the names; either binding disables the schema.
        for src in ["def target(**kw):\n    pass\n", "target = None\n"] {
            let r = with_overlay(&format!("{src}target(dri│)\n"), &["target"]);
            assert_eq!(kind(&r), "none");
            assert_eq!(
                r.reason,
                Some(Reason::CalleeRebound {
                    name: "target".to_string()
                })
            );
        }
    }

    #[test]
    fn site_empty_document() {
        let r = at("│");
        assert_eq!(kind(&r), "none");
    }

    #[test]
    fn site_inside_a_surrogate_pair() {
        let text = "x = \"😀\"\ntarget(dri)\n";
        // Column 6 is between the two halves of the emoji: no panic, and the
        // position lands on the emoji (inside the string).
        let r = at_position(text, 0, 6).expect("in range");
        assert_eq!(kind(&r), "none");
        let r = at_position(text, 1, 10).expect("in range");
        assert_eq!(kind(&r), "target_key");
    }

    #[test]
    fn site_crlf() {
        let text = "x = 1\r\ntarget(name = \"x\", dri)\r\n";
        let r = at_position(text, 1, 22).expect("in range");
        assert_eq!(kind(&r), "target_key");
        assert_eq!(r.site.prefix(), Some("dri"));
    }

    #[test]
    fn site_lone_cr() {
        // A lone `\r` ends a line for the editor; the lexer treats it as
        // whitespace. The column still lands on the right byte.
        let text = "x = 1\rtarget(name = \"x\", dri)\n";
        let r = at_position(text, 1, 22).expect("in range");
        assert_eq!(kind(&r), "target_key");
        assert_eq!(r.site.prefix(), Some("dri"));
    }

    #[test]
    fn site_bom() {
        let text = "\u{feff}target(dri)\n";
        let r = at_position(text, 0, 11).expect("in range");
        assert_eq!(kind(&r), "target_key");
        assert_eq!(r.site.prefix(), Some("dri"));
    }

    #[test]
    fn site_tabs() {
        let r = at("target(\n\tname = \"x\",\n\tdri│\n)\n");
        assert_eq!(kind(&r), "target_key");
        assert_eq!(r.site.prefix(), Some("dri"));
    }

    #[test]
    fn site_load_with_alias() {
        let r = at("load(\"x\", a = \"b│\")\n");
        assert_eq!(kind(&r), "load_symbol");
        let r = at("load(\"//pk│\", \"b\")\n");
        assert_eq!(kind(&r), "load_path");
    }

    #[test]
    fn site_kwargs_and_args_splats() {
        assert_eq!(kind(&at("target(**kw│)\n")), "none");
        assert_eq!(kind(&at("target(*a, │)\n")), "target_key");
        assert_eq!(kind(&at("target(**kw, dri│)\n")), "target_key");
    }

    #[test]
    fn site_escaped_quote() {
        let r = at("target(name = \"a\\\"b\", dri│)\n");
        assert_eq!(kind(&r), "target_key");
    }

    #[test]
    fn site_target_in_a_comment_and_in_strings() {
        assert_eq!(kind(&at("# target(\nx = dri│\n")), "none");
        assert_eq!(kind(&at("x = \"target(\"\ny = dri│\n")), "none");
        assert_eq!(kind(&at("x = 'target('\ny = dri│\n")), "none");
    }

    #[test]
    fn site_fstring() {
        let r = at("target(name = f\"x{y}\", dri│)\n");
        assert_eq!(kind(&r), "target_key");
    }

    #[test]
    fn site_line_continuation() {
        let r = at("x = 1 + \\\n    2\ntarget(dri│)\n");
        assert_eq!(kind(&r), "target_key");
    }

    #[test]
    fn site_mismatched_brackets() {
        assert_eq!(kind(&at("foo(]\ntarget(dri│\n")), "target_key");
        assert_eq!(kind(&at("x = [1, 2)\ntarget(dri│\n")), "target_key");
        let r = at("deps = [ target(│ ]\n");
        assert_eq!(kind(&r), "target_key");
    }

    #[test]
    fn site_reserved_words_and_literal_errors() {
        for fixture in [
            "with│",
            "from│",
            "import│",
            "01│",
            "\u{feff}│",
            "target(01│",
        ] {
            let r = at(fixture);
            assert!(
                matches!(kind(&r), "none" | "target_key"),
                "{fixture:?}: {r:?}"
            );
        }
        assert_eq!(kind(&at("with│")), "none");
    }

    #[test]
    fn site_resolver_on_10k_lines() {
        // Not asserted here: the 50 ms bound is `resolver_on_10k_lines_under_50ms`.
        let mut text = String::new();
        for i in 0..10_000 {
            text.push_str(&format!(
                "target(name = \"t{i}\", driver = \"exec\", cmd = \"x\")\n"
            ));
        }
        text.push_str("target(dri");
        let tokens = lex(&text);
        let o = text.len();
        let r = resolve(
            &Doc {
                text: &text,
                tokens: &tokens,
                overlay: None,
                ast: AstState::Current,
            },
            o,
        );
        assert_eq!(kind(&r), "target_key");
    }
}

#[cfg(test)]
mod cost {
    use super::super::lex::lex;
    use super::*;
    use std::time::{Duration, Instant};

    /// 10,000 lines: a clean corpus, and an adversarial one where every line
    /// is tab-indented, an unterminated string or a stray `$`.
    fn corpora() -> [(&'static str, String); 2] {
        let mut clean = String::new();
        let mut adversarial = String::new();
        for i in 0..10_000 {
            clean.push_str(&format!(
                "target(name = \"t{i}\", driver = \"exec\", cmd = \"echo {i}\")\n"
            ));
            adversarial.push_str(match i % 3 {
                0 => "\ttarget(name = \"x\")\n",
                1 => "x = \"unterminated\n",
                _ => "y = $ + 1\n",
            });
        }
        clean.push_str("target(name = \"last\", dri");
        adversarial.push_str("target(name = \"last\", dri");
        [("clean", clean), ("adversarial", adversarial)]
    }

    /// The slowest resolution over a few runs, the cursor at the end (the
    /// resolver walks every token before it).
    fn time_resolve(text: &str) -> (Duration, Resolution) {
        let tokens = lex(text);
        let doc = Doc {
            text,
            tokens: &tokens,
            overlay: None,
            ast: AstState::Current,
        };
        let mut worst = Duration::ZERO;
        let mut last = None;
        for _ in 0..3 {
            let started = Instant::now();
            let r = resolve(&doc, text.len());
            worst = worst.max(started.elapsed());
            last = Some(r);
        }
        (worst, last.unwrap())
    }

    /// A regression trips, noise does not: the bound is an order of magnitude
    /// above the measured figure (see the ignored bench below).
    #[test]
    fn resolver_on_10k_lines_under_50ms() {
        for (name, text) in corpora() {
            let (took, r) = time_resolve(&text);
            assert_eq!(r.site.kind(), "target_key", "{name}");
            assert!(
                took < Duration::from_millis(50),
                "{name}: resolving took {took:?}"
            );
        }
    }

    /// The real figures: lexing (once per edit) and resolving (once per
    /// request) on both corpora. `cargo test --release -p plugin-buildfile
    /// resolver_bench -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a test"]
    fn resolver_bench() {
        for (name, text) in corpora() {
            let started = Instant::now();
            let tokens = lex(&text);
            let lexed = started.elapsed();
            let (resolved, _) = time_resolve(&text);
            println!(
                "{name}: {} bytes, {} tokens; lex {lexed:?}, resolve {resolved:?}",
                text.len(),
                tokens.toks.len()
            );
        }
    }
}
