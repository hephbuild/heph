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
use crate::pluginbuildfile::ast::bound_names;
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

/// The enclosing call to any other function named by a plain identifier
/// (`my_rule(`, `glob(`; not `x.f(` and not a `def`'s own parameter list).
/// Whose parameters it takes is the handler's question: the resolver reads
/// tokens only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArgCall {
    pub callee: String,
    /// `Some(line)` when the extent was guessed, as for [`Call::guessed`].
    pub guessed: Option<u32>,
    /// The keyword arguments present in the extent (the one under the cursor
    /// excluded).
    pub keys: Vec<String>,
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

/// What a [`Site::Member`]'s receiver (the expression before the `.`) is, so
/// `--explain` can say why its member list is empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Receiver {
    /// A chain of names whose root the file does not bind: a global's path.
    Names,
    /// A chain of names whose root `name` the file binds itself (assigned,
    /// loaded, a parameter), so it is not the global of that name. `rest`
    /// is the chain after the root (`l.mode.│`: `["mode"]`), which typed
    /// completion folds over.
    Rebound { name: String, rest: Vec<String> },
    /// A string, list or dict literal (D9): `"-".│`, `[1].│`, `{}.│`. Read
    /// from the token before the `.`, so it needs no parse.
    Literal { kind: Literal },
    /// Anything else: a call, an index, a parenthesized group.
    Expression,
}

impl Receiver {
    fn name(&self) -> &'static str {
        match self {
            Receiver::Names => "names",
            Receiver::Rebound { .. } => "rebound",
            Receiver::Literal { .. } => "literal",
            Receiver::Expression => "expression",
        }
    }
}

/// The kind of a [`Receiver::Literal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Literal {
    /// A string token, or an implicitly concatenated run of them (bytes are
    /// not: there is no bytes type to list).
    Str,
    /// A `[…]` display: its `[` does not follow a token that ends an
    /// expression, so it is not an index.
    List,
    /// A `{…}` display.
    Dict,
}

impl Literal {
    /// The type `--explain` reports. The element types are unknown, and the
    /// display says so.
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            Literal::Str => "str",
            Literal::List => "list[Any]",
            Literal::Dict => "dict[Any, Any]",
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
    /// The string value of any other keyword directly inside `target(` /
    /// `provider_state(`: `workdir = "tr│"`. Whether heph has anything to
    /// offer depends on the field's type (its literals), which only the
    /// handlers know; everywhere else it answers as `None` does.
    FieldValue {
        call: Call,
        key: String,
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
    /// A keyword-name position inside a call to any other plain-named
    /// function.
    ArgKey {
        call: ArgCall,
        edit: Range<usize>,
        prefix: String,
    },
    /// After `heph.`.
    PluginNamespace {
        edit: Range<usize>,
        prefix: String,
    },
    /// After `heph.<plugin>.`, or on the function name. Under `heph.types`
    /// also deeper, in a driver namespace: `heph.types.oci.image.│` has
    /// `path` `["oci", "image"]`.
    PluginMember {
        plugin: String,
        /// The segments between the plugin and the member; empty but under
        /// `heph.types`.
        path: Vec<String>,
        edit: Range<usize>,
        prefix: String,
    },
    /// Any other member position, right after a `.`: `typing.│`,
    /// `heph.core.os.│`, `l.na│`, `f().│`. `path` is the receiver when it is
    /// a chain of identifiers whose root is a global the file does not rebind
    /// (`["typing"]`, [`Receiver::Names`]), and `None` otherwise: a local
    /// value, a call, a literal, whose members nothing here knows.
    Member {
        path: Option<Vec<String>>,
        receiver: Receiver,
        /// The names of the `def`s the cursor is inside, outermost first
        /// ([`def_path`]); empty at module level.
        scope: Vec<String>,
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
            Site::FieldValue { .. } => "field_value",
            Site::TargetKey { .. } => "target_key",
            Site::StateKey { .. } => "state_key",
            Site::ArgKey { .. } => "arg_key",
            Site::PluginNamespace { .. } => "plugin_namespace",
            Site::PluginMember { .. } => "plugin_member",
            Site::Member { .. } => "member",
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
            | Site::FieldValue { call, .. }
            | Site::TargetKey { call, .. }
            | Site::StateKey { call, .. } => Some(call),
            _ => None,
        }
    }

    /// The keyword argument the cursor sits in the value of.
    pub(crate) fn keyword(&self) -> Option<&str> {
        match self {
            Site::DriverValue { .. } => Some("driver"),
            Site::ProviderValue { .. } => Some("provider"),
            Site::FieldValue { key, .. } => Some(key),
            _ => None,
        }
    }

    /// The text before the cursor that completion filters by.
    pub(crate) fn prefix(&self) -> Option<&str> {
        match self {
            Site::None | Site::Callee { .. } => None,
            Site::DriverValue { value, .. }
            | Site::ProviderValue { value, .. }
            | Site::FieldValue { value, .. }
            | Site::Address { value }
            | Site::LoadPath { value }
            | Site::LoadSymbol { value, .. } => Some(&value.prefix),
            Site::TargetKey { prefix, .. }
            | Site::StateKey { prefix, .. }
            | Site::ArgKey { prefix, .. }
            | Site::PluginNamespace { prefix, .. }
            | Site::PluginMember { prefix, .. }
            | Site::Member { prefix, .. } => Some(prefix),
        }
    }

    /// Where the enclosing call's extent was guessed, for any call site.
    fn guessed(&self) -> Option<u32> {
        match self {
            Site::ArgKey { call, .. } => call.guessed,
            _ => self.call().and_then(|c| c.guessed),
        }
    }

    pub(crate) fn value(&self) -> Option<&Str> {
        match self {
            Site::DriverValue { value, .. }
            | Site::ProviderValue { value, .. }
            | Site::FieldValue { value, .. }
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
        if let (Site::ArgKey { call, .. }, Some(map)) = (self, out.as_object_mut()) {
            map.insert("call".to_string(), json!(call.callee));
            map.insert("present_keys".to_string(), json!(call.keys));
        }
        if let (Some(value), Some(map)) = (self.value(), out.as_object_mut()) {
            map.insert("quote".to_string(), json!(value.quote.name()));
            map.insert("edit".to_string(), span(&value.edit));
            map.insert("token".to_string(), span(&value.token));
        }
        let word = match self {
            Site::TargetKey { edit, .. }
            | Site::StateKey { edit, .. }
            | Site::ArgKey { edit, .. }
            | Site::PluginNamespace { edit, .. }
            | Site::PluginMember { edit, .. }
            | Site::Member { edit, .. } => Some(edit),
            _ => None,
        };
        if let (Some(edit), Some(map)) = (word, out.as_object_mut()) {
            map.insert("edit".to_string(), span(edit));
            map.insert("token".to_string(), span(edit));
        }
        if let (Site::PluginMember { plugin, path, .. }, Some(map)) = (self, out.as_object_mut()) {
            map.insert("plugin".to_string(), json!(plugin));
            if !path.is_empty() {
                map.insert("path".to_string(), json!(path));
            }
        }
        if let (
            Site::Member {
                path,
                receiver,
                scope,
                ..
            },
            Some(map),
        ) = (self, out.as_object_mut())
        {
            map.insert("path".to_string(), json!(path));
            map.insert("receiver".to_string(), json!(receiver.name()));
            map.insert("scope".to_string(), json!(scope));
            match receiver {
                Receiver::Rebound { name, .. } => {
                    map.insert("rebound".to_string(), json!(name));
                }
                Receiver::Literal { kind } => {
                    map.insert("type".to_string(), json!(kind.type_name()));
                }
                Receiver::Names | Receiver::Expression => {}
            }
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
    fn guessed_extent(&self, open: usize) -> usize {
        let indent = self.indent_of(open);
        let mut ls = line_end(self.text, open) + 1;
        while ls < self.text.len() {
            let le = line_end(self.text, ls);
            let line = self.text.get(ls..le).unwrap_or("");
            if let Some(i) = line.find(|c: char| c != ' ' && c != '\t' && c != '\r') {
                let first = line.as_bytes().get(i).copied().unwrap_or(b' ');
                let skipped = first == b'#' || self.inside_string(ls) || self.inside_string(ls + i);
                if !skipped && i <= indent && !matches!(first, b')' | b']' | b'}') {
                    return ls;
                }
            }
            ls = le + 1;
        }
        // Runs to EOF, the end of the text included (a cursor there is still
        // inside the call).
        self.text.len() + 1
    }

    /// The end of opener `i`'s extent, and the guessed line when it was not
    /// closed by a matching closer.
    fn extent(&self, i: usize) -> (usize, Option<u32>) {
        let Some(t) = self.tok(i) else {
            return (self.text.len(), None);
        };
        if t.link == NO_LINK {
            let end = self.guessed_extent(t.start as usize);
            return (end, Some(self.line_of(end)));
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
                .is_none_or(|t| t.link != NO_LINK || self.guessed_extent(t.start as usize) > o)
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
        let selector = match callee {
            Builtin::Target => "driver",
            Builtin::ProviderState => "provider",
            Builtin::Load => "",
        };
        let (keys, selected) = self.keys_in(i, end, cursor, selector);
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

    /// The call to a plain-named non-builtin function whose `(` is token `i`.
    fn arg_call_at(&self, i: usize, cursor: Option<usize>) -> Option<ArgCall> {
        let t = self.tok(i)?;
        if t.kind != Kind::Open(Bracket::Round) {
            return None;
        }
        let c = self.tok(i.checked_sub(1)?)?;
        if c.kind != Kind::Ident || Builtin::of(self.text_of(c)).is_some() {
            return None;
        }
        // Not a member call (`x.f(`), and not a `def`'s parameter list.
        if let Some(before) = i.checked_sub(2).and_then(|j| self.tok(j))
            && (before.kind == Kind::Dot
                || (before.kind == Kind::Keyword && self.text_of(before) == "def"))
        {
            return None;
        }
        let (end, guessed) = self.extent(i);
        let (keys, _) = self.keys_in(i, end, cursor, "");
        Some(ArgCall {
            callee: self.text_of(c).to_string(),
            guessed,
            keys,
        })
    }

    /// The keyword names written directly inside the call whose `(` is token
    /// `i` (up to `end`), the one under `cursor` excluded; and the string
    /// value of the `selector` keyword, if any.
    fn keys_in(
        &self,
        i: usize,
        end: usize,
        cursor: Option<usize>,
        selector: &str,
    ) -> (Vec<String>, Option<String>) {
        let mut keys = Vec::new();
        let mut selected = None;
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
        (keys, selected)
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

    /// Whether byte `pos` lies inside a token (a comment, a string).
    fn covered(&self, pos: usize) -> bool {
        let i = self.toks().partition_point(|t| (t.start as usize) <= pos);
        i > 0 && self.tok(i - 1).is_some_and(|t| (t.end as usize) > pos)
    }

    /// Whether the `\n` at byte `n` is escaped by a `\` outside any token
    /// (CRLF included): the lexer skips it, so it is not a line break.
    fn escaped_newline(&self, n: usize) -> bool {
        let before = self.text.get(..n).unwrap_or("");
        let before = before.strip_suffix('\r').unwrap_or(before);
        before.ends_with('\\') && !self.covered(before.len() - 1)
    }

    /// Whether a line break the lexer reads as one lies between token `a`
    /// and token `b`: a `\n` not escaped by a `\`. Only comments lie between
    /// two adjacent significant tokens, and a comment runs to its line end,
    /// so a `\` inside one escapes nothing.
    fn newline_between(&self, a: usize, b: usize) -> bool {
        let (Some(a), Some(b)) = (self.tok(a), self.tok(b)) else {
            return false;
        };
        let from = a.end as usize;
        self.text
            .get(from..b.start as usize)
            .unwrap_or("")
            .match_indices('\n')
            .any(|(n, _)| !self.escaped_newline(from + n))
    }

    /// Where opener `t` stops enclosing: a position is inside it while it
    /// is below this. Through the byte of its (possibly mismatched) closer,
    /// so a line that begins with the closer begins inside. `None` for an
    /// unclosed one, which [`def_path`] ends itself.
    fn open_until(&self, t: &Tok) -> Option<usize> {
        if t.link == NO_LINK {
            return None;
        }
        self.tok(t.link as usize).map(|c| c.start as usize + 1)
    }

    /// Whether token `t` is a keyword that only ever starts a statement, so
    /// a line it starts cannot continue an expression.
    fn starts_statement(&self, t: &Tok) -> bool {
        t.kind == Kind::Load
            || t.kind == Kind::Keyword
                && matches!(
                    self.text_of(t),
                    "def" | "return" | "pass" | "break" | "continue" | "elif"
                )
    }

    /// An error token made of nothing but blanks and line breaks (a BOM, a
    /// form feed, a bad dedent's `\n   `): it is whitespace to the line
    /// structure.
    fn is_blank_token(&self, t: &Tok) -> bool {
        matches!(t.kind, Kind::Error(_))
            && self.text_of(t).chars().all(|c| c == '\n' || is_blank(c))
    }

    /// Whether the `def` at token `def` has its header's `:` (the first one
    /// outside its brackets) ending at or before `o`: the cursor is in a
    /// one-line body, `def g(): return l.│`.
    fn header_closed_before(&self, def: usize, o: usize) -> bool {
        let mut depth = 0usize;
        for t in self.toks().get(def + 1..).unwrap_or(&[]) {
            if t.start as usize >= o {
                break;
            }
            match t.kind {
                Kind::Open(_) => depth += 1,
                Kind::Close(_) => depth = depth.saturating_sub(1),
                Kind::Colon if depth == 0 => return t.end as usize <= o,
                _ => {}
            }
        }
        false
    }
}

/// A blank character for the line structure: indentation (`' '`, `'\t'`),
/// and the bytes the lexer ignores there (`'\r'`, a form feed, a BOM).
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\x0c' | '\u{feff}')
}

/// The first token of a logical line, before the cursor.
struct Head<'d> {
    tok: usize,
    /// Its indent in bytes: the spaces and tabs before it on its line.
    indent: usize,
    /// The name, when the line is `def NAME`.
    def: Option<&'d str>,
}

/// The names of the `def`s whose body holds byte `o`, outermost first (D6).
/// From the tokens only, so it answers on a document that does not parse.
///
/// The cursor is inside `def NAME` when a `def` token starts a logical line
/// at indent *k*, the cursor's logical line is deeper than *k*, and no
/// logical line between them is at indent ≤ *k*. A logical line starts at a
/// token that is the first on its line, outside every bracket, and not after
/// a `\` continuation; so blank and comment-only lines, and lines that begin
/// inside a string, a bracket or a continuation, are not lines here.
///
/// An unclosed bracket encloses what follows it, as it does for the lexer,
/// until a line starts with a keyword no expression continues with (`def`,
/// `return`, `pass`, `break`, `continue`, `elif`, `load`). It does not end
/// at the call's guessed extent, as [`Doc::open_stack`] has it: a call whose
/// arguments sit at column 0 would end at the first of them as soon as its
/// `)` is deleted.
///
/// The cursor's own logical line is the one its line continues, if it
/// continues one; otherwise its indent is the blanks before the cursor, so a
/// cursor at column 0 of a blank line in a body is at module level. On a
/// `def` line the cursor is in the outer scope, until the header's `:`: after
/// it, `def g(): return l.│` is in `g`.
///
/// Indents are byte counts of spaces and tabs (the lexer rejects a tab
/// indent, so a mix only arises in a broken document); `\r`, a form feed and
/// a BOM count as nothing. `o` is a byte offset: a client's UTF-16 position
/// is converted before.
pub(crate) fn def_path(doc: &Doc<'_>, o: usize) -> Vec<String> {
    let text = doc.text;
    let o = o.min(text.len());
    let cl = line_start(text, o);
    let mut heads: Vec<Head<'_>> = Vec::new();
    // Where each enclosing opener stops enclosing; `None` while unclosed.
    let mut stack: Vec<Option<usize>> = Vec::new();
    let open_at =
        |stack: &[Option<usize>], at: usize| stack.iter().any(|until| until.is_none_or(|u| u > at));
    // Whether the cursor's line begins inside a bracket, taken when the walk
    // reaches that line.
    let mut cursor_line_bracketed: Option<bool> = None;
    // The end of the last token that is not blank.
    let mut prev_end: Option<usize> = None;
    for (i, t) in doc.toks().iter().enumerate() {
        let s = t.start as usize;
        if s >= o {
            break;
        }
        if doc.is_blank_token(t) {
            continue;
        }
        let gap = text.get(prev_end.unwrap_or(0)..s).unwrap_or("");
        // The first token on its line: a line break since the last token (or
        // no token before it at all), then only blanks.
        let lead = match gap.rfind('\n') {
            Some(n) => gap.get(n + 1..),
            None => prev_end.is_none().then_some(gap),
        };
        prev_end = Some(t.end as usize);
        if t.kind == Kind::Comment {
            continue;
        }
        let lead = lead.filter(|l| {
            l.chars().all(is_blank)
                && !(s - l.len())
                    .checked_sub(1)
                    .is_some_and(|n| doc.escaped_newline(n))
        });
        // An unclosed bracket runs on, as the lexer has it (no newline
        // inside), until a line starts with a statement keyword, which no
        // expression continues with: `def g():` after a broken call.
        if lead.is_some() && doc.starts_statement(t) {
            stack.retain(Option::is_some);
        }
        if cursor_line_bracketed.is_none() && s >= cl {
            cursor_line_bracketed = Some(open_at(&stack, cl));
        }
        stack.retain(|until| until.is_none_or(|u| u > s));
        if stack.is_empty()
            && let Some(lead) = lead
        {
            let def = (t.kind == Kind::Keyword && doc.text_of(t) == "def")
                .then(|| doc.tok(i + 1))
                .flatten()
                .filter(|n| n.kind == Kind::Ident)
                .map(|n| doc.text_of(n));
            heads.push(Head {
                tok: i,
                indent: lead.bytes().filter(|b| matches!(b, b' ' | b'\t')).count(),
                def,
            });
        }
        if matches!(t.kind, Kind::Open(_)) {
            stack.push(doc.open_until(t));
        }
    }
    let bracketed = cursor_line_bracketed.unwrap_or_else(|| open_at(&stack, cl));
    let continued = bracketed
        || doc.inside_string(cl)
        || cl.checked_sub(1).is_some_and(|n| doc.escaped_newline(n));
    let start_of = |h: &Head<'_>| doc.tok(h.tok).map_or(0, |t| t.start as usize);
    // The cursor's own logical line: the head it continues, or its own head
    // when one starts on its line before it.
    let own = if continued {
        heads.len().checked_sub(1)
    } else {
        heads
            .last()
            .filter(|h| start_of(h) >= cl)
            .map(|_| heads.len() - 1)
    };
    let indent = match own.and_then(|p| heads.get(p)) {
        Some(h) if continued => h.indent,
        _ => text
            .get(cl..o)
            .unwrap_or("")
            .chars()
            .take_while(|&c| is_blank(c))
            .filter(|c| matches!(c, ' ' | '\t'))
            .count(),
    };
    let one_line = own
        .and_then(|p| heads.get(p))
        .and_then(|h| h.def.filter(|_| doc.header_closed_before(h.tok, o)));
    let mut path: Vec<String> = Vec::new();
    let mut m = indent;
    for h in heads
        .get(..own.unwrap_or(heads.len()))
        .unwrap_or(&[])
        .iter()
        .rev()
    {
        if m == 0 {
            break;
        }
        if h.indent < m {
            if let Some(name) = h.def {
                path.push(name.to_string());
            }
            m = h.indent;
        }
    }
    path.reverse();
    path.extend(one_line.map(str::to_string));
    path
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
    // Shadowing, decided here for every feature. Every site inside a
    // `target(` / `provider_state(` call is shadowed with its callee, so the
    // list is `Site::call`'s, not a second copy that a new kind can miss. A
    // rebound `heph` never gets here as a `heph.` site: `dotted_site` makes it
    // a `member` whose receiver is rebound, like any other bound root.
    let callee = resolution.site.call().map(|call| call.callee.name());
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
    if let Some(line) = resolution.site.guessed() {
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

    // Any member position, right after a `.`: `heph.`, `heph.<plugin>.`,
    // `typing.`, `f().`.
    if let Some(site) = dotted_site(doc, word, o, stack.is_empty(), &edit, &prefix) {
        return Ok(site);
    }

    let Some((open_idx, call)) = enclosing_call else {
        return innermost
            .and_then(|i| arg_key_site(doc, i, o, word_tok, word_start, edit, prefix))
            .ok_or(Reason::NotASite);
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
                // Another field's value is a site only once it is a string
                // (`string_site`): before one, the position is where a
                // variable or an expression starts, which the stock answers.
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

/// A keyword-name position (right after the `(` or a `,`, on an identifier or
/// nothing) directly inside a call to a plain-named non-builtin function whose
/// `(` is token `open_idx`.
fn arg_key_site(
    doc: &Doc<'_>,
    open_idx: usize,
    o: usize,
    word_tok: Option<Tok>,
    word_start: usize,
    edit: Range<usize>,
    prefix: String,
) -> Option<Site> {
    if word_tok.is_some_and(|t| t.kind != Kind::Ident) {
        return None;
    }
    let prev = doc.prev_significant(word_start, Some(open_idx));
    if !matches!(
        prev.and_then(|i| doc.tok(i)).map(|t| t.kind),
        None | Some(Kind::Comma)
    ) {
        return None;
    }
    let call = doc.arg_call_at(open_idx, Some(o))?;
    Some(Site::ArgKey { call, edit, prefix })
}

fn key_site(call: Call, edit: Range<usize>, prefix: String) -> Site {
    match call.callee {
        Builtin::ProviderState => Site::StateKey { call, edit, prefix },
        _ => Site::TargetKey { call, edit, prefix },
    }
}

/// The site of a member position: the member being typed (or the cursor)
/// follows a `.`. `heph.│` is [`Site::PluginNamespace`], `heph.<plugin>.│`
/// (and deeper under `heph.types`) [`Site::PluginMember`], any other
/// [`Site::Member`]. `None` when the previous significant token is not a `.`.
///
/// Whitespace, newlines and comments between the tokens do not matter: a `.`
/// token only ever means attribute access (a float is one `Number` token), so
/// `typing. A│`, `typing.\n    A│` and `typing\n  .│` all have the receiver
/// `typing`.
///
/// `top_level` is whether the cursor is outside every bracket, which decides
/// whether a line break before a `[` is a `Newline` token (D9).
fn dotted_site(
    doc: &Doc<'_>,
    word: Option<usize>,
    o: usize,
    top_level: bool,
    edit: &Range<usize>,
    prefix: &str,
) -> Option<Site> {
    let member_start = word
        .and_then(|i| doc.tok(i))
        .map_or(o, |t| t.start as usize);
    let dot = doc.prev_significant(member_start, None)?;
    if doc.tok(dot)?.kind != Kind::Dot {
        return None;
    }
    let chain = ident_chain(doc, dot);
    let edit = edit.clone();
    let prefix = prefix.to_string();
    let scope = def_path(doc, o);
    // A root the file binds itself (assigned, loaded, a parameter) is not the
    // global of that name, whatever it holds; that holds for `heph` too.
    let rebound = chain
        .as_deref()
        .and_then(|segs| segs.split_first())
        .filter(|(root, _)| is_rebound(doc.overlay, root));
    if let Some((root, rest)) = rebound {
        return Some(Site::Member {
            path: None,
            receiver: Receiver::Rebound {
                name: (*root).to_string(),
                rest: rest.iter().map(|s| (*s).to_string()).collect(),
            },
            scope,
            edit,
            prefix,
        });
    }
    Some(match chain.as_deref() {
        Some(["heph"]) => Site::PluginNamespace { edit, prefix },
        // A driver's `Kwargs` nests one namespace per segment of its name.
        Some(["heph", plugin, path @ ..]) if path.is_empty() || *plugin == "types" => {
            Site::PluginMember {
                plugin: (*plugin).to_string(),
                path: path.iter().map(|s| (*s).to_string()).collect(),
                edit,
                prefix,
            }
        }
        Some(segs) => Site::Member {
            path: Some(segs.iter().map(|s| (*s).to_string()).collect()),
            receiver: Receiver::Names,
            scope,
            edit,
            prefix,
        },
        None => Site::Member {
            path: None,
            receiver: literal_receiver(doc, dot, top_level)
                .map_or(Receiver::Expression, |kind| Receiver::Literal { kind }),
            scope,
            edit,
            prefix,
        },
    })
}

/// Whether a comprehension's `for NAME … in` or a `lambda NAME … :` binds
/// `name` in the bracket groups enclosing `o`, or on `o`'s line (I8). The
/// fork reports such a binding under the enclosing def, and not at module
/// level at all, so a typed answer for `name` there could be another
/// binding's: `[x.│ for x in xs]` must not take a module-level `x`.
///
/// Read from the tokens over the outermost enclosing group's extent and the
/// cursor's line, so it over-approximates (a `lambda` elsewhere in the group
/// counts): erring toward an empty answer, never toward a wrong one.
pub(crate) fn bound_in_group(doc: &Doc<'_>, o: usize, name: &str) -> bool {
    let mut lo = line_start(doc.text, o);
    let mut hi = line_end(doc.text, o);
    if let Some(&outer) = doc.open_stack(o).first()
        && let Some(t) = doc.tok(outer)
    {
        lo = lo.min(t.start as usize);
        hi = hi.max(doc.extent(outer).0);
    }
    let toks = doc.toks();
    let mut i = doc.first_at_or_after(lo);
    // Inside a binder: the token that ends its names (`in` for `for`, `:`
    // for `lambda`).
    let mut binder: Option<&str> = None;
    while let Some(t) = toks.get(i).filter(|t| (t.start as usize) < hi) {
        let text = doc.text_of(t);
        match (binder, t.kind) {
            (None, Kind::Keyword) if text == "for" => binder = Some("in"),
            (None, Kind::Keyword) if text == "lambda" => binder = Some(":"),
            (Some("in"), Kind::Keyword) if text == "in" => binder = None,
            (Some(":"), Kind::Colon) => binder = None,
            (Some(_), Kind::Ident) if text == name => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// The literal the `.` token `dot` follows (D9), or `None` when its receiver
/// is anything else.
///
/// - A string token is `str`; in an implicitly concatenated run it is the
///   last one, so the run is `str` too. A bytes literal is nothing.
/// - A `}` is a dict.
/// - A `]` is a list when its `[` opens a display: the token before the `[`
///   does not end an expression. At top level a line break before the `[`
///   is a `Newline` token, which ends nothing; inside brackets the lexer
///   emits none, so the `[` indexes what precedes it.
///
/// The closer's opener is one hop over the lexer's bracket link, so the cost
/// does not grow with the literal; a link that is missing, or that pairs
/// two kinds, answers nothing.
fn literal_receiver(doc: &Doc<'_>, dot: usize, top_level: bool) -> Option<Literal> {
    let dot_start = doc.tok(dot)?.start as usize;
    let r = doc.prev_significant(dot_start, None)?;
    let rt = *doc.tok(r)?;
    let bracket = match rt.kind {
        Kind::Str => {
            let text = doc.text_of(&rt);
            let prefix = text.get(..text.find(['"', '\''])?)?;
            return (!prefix.contains(['b', 'B'])).then_some(Literal::Str);
        }
        Kind::Close(b @ (Bracket::Square | Bracket::Curly)) => b,
        _ => return None,
    };
    let open = rt.link as usize;
    let ot = doc.tok(open).filter(|t| {
        rt.link != NO_LINK && t.kind == Kind::Open(bracket) && !t.popped && t.link as usize == r
    })?;
    if bracket == Bracket::Curly {
        return Some(Literal::Dict);
    }
    let Some(p) = doc.prev_significant(ot.start as usize, None) else {
        return Some(Literal::List);
    };
    let pt = doc.tok(p)?;
    let ends_expression = matches!(
        pt.kind,
        Kind::Ident | Kind::Str | Kind::Number | Kind::Close(_)
    );
    if !ends_expression || (top_level && doc.newline_between(p, open)) {
        Some(Literal::List)
    } else {
        None
    }
}

/// The `a.b.c` chain of identifiers ending at the `.` token `dot`, or `None`
/// when the receiver is anything else (a call, an index, a literal). It walks
/// left over `Ident Dot` pairs by token order, skipping comments, whatever
/// whitespace lies between them.
fn ident_chain<'d>(doc: &'d Doc<'_>, dot: usize) -> Option<Vec<&'d str>> {
    let mut segments: Vec<&str> = Vec::new();
    let mut dot_tok = *doc.tok(dot)?;
    loop {
        // `x[0].a`, `f().a`, `"s".a`: a receiver that is not a name.
        let name = doc
            .prev_significant(dot_tok.start as usize, None)
            .and_then(|j| doc.tok(j))
            .copied()?;
        if name.kind != Kind::Ident {
            return None;
        }
        segments.push(doc.text_of(&name));
        match doc
            .prev_significant(name.start as usize, None)
            .and_then(|j| doc.tok(j))
        {
            Some(b) if b.kind == Kind::Dot => dot_tok = *b,
            _ => break,
        }
    }
    segments.reverse();
    Some(segments)
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
            // Any other keyword's string value, unless it is an address
            // (`deps = "//x"`), which keeps its own site below.
            (Builtin::Target | Builtin::ProviderState, Some(key))
                if direct && !is_address_shaped(&doc.str_at(i, o, is_address_byte)?) =>
            {
                let value = doc.str_at(i, o, is_name_byte)?;
                return Some(Site::FieldValue {
                    key: key.to_string(),
                    call,
                    value,
                });
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
    is_address_shaped(&value).then_some(Site::Address { value })
}

/// Whether a string's text before the cursor starts an address.
fn is_address_shaped(value: &Str) -> bool {
    value.prefix.starts_with("//") || value.prefix.starts_with(':')
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
    fn site_plugin_member_path_only_under_types() {
        let member = |fixture: &str| match at(fixture).site {
            Site::PluginMember {
                plugin,
                path,
                prefix,
                ..
            } => Some((plugin, path, prefix)),
            _ => None,
        };
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        assert_eq!(
            member("x = heph.types.│\n"),
            Some(("types".to_string(), s(&[]), String::new()))
        );
        assert_eq!(
            member("def f(**kw: heph.types.oci.image.Kw│):\n"),
            Some(("types".to_string(), s(&["oci", "image"]), "Kw".to_string()))
        );
        assert_eq!(
            member("x = heph.types.bash.Kwa│rgs\n"),
            Some(("types".to_string(), s(&["bash"]), "Kwa".to_string())),
            "on the member, the prefix runs to the cursor"
        );
        assert_eq!(
            member("x = heph.fs.│\n"),
            Some(("fs".to_string(), s(&[]), String::new()))
        );
        assert_eq!(
            member("x = heph.fs.join.│\n"),
            None,
            "a plugin function has no members"
        );
    }

    /// Every other position after a `.` is a `member` site; its path is the
    /// receiver when that is a chain of names rooted at a global.
    #[test]
    fn site_member_path_is_a_global_name_chain() {
        let path = |r: Resolution| match r.site {
            Site::Member {
                path,
                receiver,
                prefix,
                ..
            } => Some((path, receiver, prefix)),
            _ => None,
        };
        let s = |v: &[&str]| Some(v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>());
        let names = Receiver::Names;
        let expr = Receiver::Expression;
        for (fixture, want, receiver, prefix) in [
            ("x = typing.│\n", s(&["typing"]), &names, ""),
            ("def f(x: typing.Ca│ll):\n", s(&["typing"]), &names, "Ca"),
            ("x = a.b.│\n", s(&["a", "b"]), &names, ""),
            (
                "x = heph.fs.join.│\n",
                s(&["heph", "fs", "join"]),
                &names,
                "",
            ),
            ("x = typing.Any.│\n", s(&["typing", "Any"]), &names, ""),
            ("x = f().│\n", None, &expr, ""),
            ("x = l[0].│\n", None, &expr, ""),
            (
                "x = \"s\".jo│\n",
                None,
                &Receiver::Literal { kind: Literal::Str },
                "jo",
            ),
            // Whitespace, newlines and comments between the tokens do not
            // matter: a `.` only ever means attribute access.
            ("x = typing. A│\n", s(&["typing"]), &names, "A"),
            ("x = (typing.\n    A│)\n", s(&["typing"]), &names, "A"),
            ("x = (typing\n  .│)\n", s(&["typing"]), &names, ""),
            ("x = (a.\n typing.│)\n", s(&["a", "typing"]), &names, ""),
            ("x = (typing # c\n  .│)\n", s(&["typing"]), &names, ""),
            ("x = f() . │\n", None, &expr, ""),
            // Inside a call, a value or a default: still a member.
            ("foo(typing.│)\n", s(&["typing"]), &names, ""),
            (
                "target(name = \"x\", deps = typing.│)\n",
                s(&["typing"]),
                &names,
                "",
            ),
            ("def f(x = typing.│):\n", s(&["typing"]), &names, ""),
        ] {
            assert_eq!(
                path(at(fixture)),
                Some((want, receiver.clone(), prefix.to_string())),
                "{fixture}"
            );
        }
        let rebound_chain = |name: &str, rest: &[&str]| {
            Some((
                None,
                Receiver::Rebound {
                    name: name.to_string(),
                    rest: rest.iter().map(|x| (*x).to_string()).collect(),
                },
                String::new(),
            ))
        };
        let rebound = |name: &str| rebound_chain(name, &[]);
        assert_eq!(
            path(with_overlay("x = typing.│\n", &["typing"])),
            rebound("typing"),
            "a root the file binds is not the global"
        );
        assert_eq!(
            path(with_overlay(
                "load(\"//x:y.star\", typing = \"foo\")\nx = typing.│\n",
                &["typing"]
            )),
            rebound("typing"),
            "a load binds its alias"
        );
        assert_eq!(
            path(with_overlay(
                "def f(typing):\n    x = typing.│\n",
                &["f", "typing"]
            )),
            rebound("typing"),
            "a parameter binds its name"
        );
        assert_eq!(
            path(with_overlay("x = typing.│\n", &["other"])),
            Some((s(&["typing"]), Receiver::Names, String::new()))
        );
        // A rebound `heph` is not the global either: no plugin sites, and no
        // callee shadowing that would hand `heph.` to the stock global list.
        for (fixture, rest) in [
            ("x = heph.│\n", &[][..]),
            ("x = heph.core.│\n", &["core"][..]),
            ("x = heph.core.os.│\n", &["core", "os"][..]),
        ] {
            let r = with_overlay(fixture, &["heph"]);
            assert_eq!(r.reason, None, "{fixture}");
            assert_eq!(path(r), rebound_chain("heph", rest), "{fixture}");
        }
        for not_member in [
            "# typing.│\n",
            "x = \"typing.│\"\n",
            "x = 1.│\n",
            "x = typ│\n",
        ] {
            assert_eq!(path(at(not_member)), None, "{not_member}");
        }
    }

    #[test]
    fn site_member_explain_json_names_the_receiver() {
        let json = |r: Resolution| r.site.to_json();
        let names = json(at("x = typing.│\n"));
        assert_eq!(names["kind"], json!("member"));
        assert_eq!(names["path"], json!(["typing"]));
        assert_eq!(names["receiver"], json!("names"));
        assert!(names.get("rebound").is_none(), "{names}");
        let rebound = json(with_overlay("x = typing.│\n", &["typing"]));
        assert_eq!(rebound["path"], Value::Null);
        assert_eq!(rebound["receiver"], json!("rebound"));
        assert_eq!(rebound["rebound"], json!("typing"));
        let expr = json(at("x = f().│\n"));
        assert_eq!(expr["path"], Value::Null);
        assert_eq!(expr["receiver"], json!("expression"));
        assert!(expr.get("rebound").is_none(), "{expr}");
        assert!(expr.get("type").is_none(), "{expr}");
        assert_eq!(expr["scope"], json!([]));
        for (fixture, ty) in [
            ("x = \"-\".│\n", "str"),
            ("x = [1].│\n", "list[Any]"),
            ("x = {}.│\n", "dict[Any, Any]"),
        ] {
            let literal = json(at(fixture));
            assert_eq!(literal["receiver"], json!("literal"), "{fixture}");
            assert_eq!(literal["type"], json!(ty), "{fixture}");
            assert_eq!(literal["path"], Value::Null, "{fixture}");
        }
        let scoped = json(at("def f(l):\n    def g(m):\n        m.│\n"));
        assert_eq!(scoped["scope"], json!(["f", "g"]));
    }

    /// The receiver of the member site at the `│`, or `None` when it is not
    /// a member site.
    fn receiver(fixture: &str) -> Option<Receiver> {
        match at(fixture).site {
            Site::Member { receiver, .. } => Some(receiver),
            _ => None,
        }
    }

    /// D9's adversarial inputs: the literal before the `.`, from the token
    /// before it and one bracket-link hop.
    #[test]
    fn site_literal_receiver() {
        let lit = |kind| Some(Receiver::Literal { kind });
        let (str_, list, dict) = (lit(Literal::Str), lit(Literal::List), lit(Literal::Dict));
        let expr = Some(Receiver::Expression);
        for (fixture, want) in [
            ("x = \"a\".│\n", &str_),
            ("x = 'a'.│\n", &str_),
            ("x = \"\"\"a\"\"\".│\n", &str_),
            ("x = r\"a\".│\n", &str_),
            ("x = \"a\" \"b\".│\n", &str_),
            ("x = \"a\"\n  \"b\".│\n", &str_),
            ("x = f\"x{y}\".│\n", &str_),
            ("x = \"]\".│\n", &str_),
            ("x = \"a\".up│\n", &str_),
            ("x = \"a\".join(xs).│\n", &expr),
            ("x = [].│\n", &list),
            ("x = [x for x in y].│\n", &list),
            ("x = [\n  1,\n].│\n", &list),
            ("x = [1, # ]\n].│\n", &list),
            ("def f():\n    return [1].│\n", &list),
            ("x = ([1]).│\n", &expr),
            ("x = [[1]][0].│\n", &expr),
            ("x = xs[0].│\n", &expr),
            ("x = f()[0].│\n", &expr),
            ("x = d[\"k\"].│\n", &expr),
            ("x = \"ab\"[0].│\n", &expr),
            ("x = {}.│\n", &dict),
            ("x = {\"a\": 1}.│\n", &dict),
            ("x = {k: v for k, v in x}.│\n", &dict),
            ("x = {\"a\": 1}[\"a\"].│\n", &expr),
            ("x = (\"a\").│\n", &expr),
            // At top level the line break is a `Newline` token, which ends
            // nothing: a new statement's display.
            ("x = xs[0]\n[1, 2].│\n", &list),
            ("x = xs[0] # c\n[1, 2].│\n", &list),
            // A `\` makes the next line part of this one: an index.
            ("x = xs[0] \\\n[1, 2].│\n", &expr),
            // Inside brackets the lexer emits no newline: an index.
            ("f(xs[0]\n  [1].│\n", &expr),
            ("x = [1].│", &list),
            ("foo([1].│\n", &list),
            ("x = y in [1].│\n", &list),
            ("x = not [1].│\n", &list),
            ("x = a if b else [1].│\n", &list),
            ("f = lambda: [1].│\n", &list),
            ("x = None[0].│\n", &expr),
            ("x = True[0].│\n", &expr),
            ("x = False[0].│\n", &expr),
            // A closer whose link pairs it with nothing, or with another kind.
            ("x = (1].│\n", &expr),
            ("x = {1].│\n", &expr),
            ("x = [1}.│\n", &expr),
            ("x = (1}.│\n", &expr),
            // Not a member site at all.
            ("x = 1.│\n", &None),
            ("x = \"ab.│\n", &None),
            ("x = \"a.│\"\n", &None),
            ("# [1].│\n", &None),
            ("x = 1 # \"a\".│\n", &None),
        ] {
            assert_eq!(&receiver(fixture), want, "{fixture:?}");
        }
        // Bytes have no type to list: whatever the dialect makes of them, not
        // a `str`.
        assert_ne!(receiver("x = b\"a\".│\n"), str_);
        assert_ne!(receiver("x = rb\"a\".│\n"), str_);
        // The prefix is the member being typed.
        assert_eq!(at("x = \"a\".up│\n").site.prefix(), Some("up"));
    }

    fn scope_of(text: &str, o: usize) -> Vec<String> {
        let tokens = lex(text);
        def_path(
            &Doc {
                text,
                tokens: &tokens,
                overlay: None,
                ast: AstState::Current,
            },
            o,
        )
    }

    fn scope(fixture: &str) -> Vec<String> {
        let (text, o) = split(fixture);
        scope_of(&text, o)
    }

    #[test]
    fn site_comprehension_and_lambda_binders() {
        let bound = |fixture: &str, name: &str| {
            let (text, o) = split(fixture);
            let tokens = lex(&text);
            let doc = Doc {
                text: &text,
                tokens: &tokens,
                overlay: None,
                ast: AstState::Current,
            };
            bound_in_group(&doc, o, name)
        };
        for (fixture, name, want) in [
            ("y = [x.│ for x in xs]\n", "x", true),
            ("y = [x.│ for x in xs]\n", "xs", false),
            ("y = [x.│ for (a, x) in xs]\n", "x", true),
            ("y = {k: v.│ for k, v in d.items()}\n", "v", true),
            ("y = f(\n    [x.│\n     for x in xs],\n)\n", "x", true),
            ("f = lambda x: x.│\n", "x", true),
            ("f = lambda a, b = 1: b.│\n", "b", true),
            ("f = lambda a: x.│\n", "x", false),
            ("y = [z for x in xs]\nx.│\n", "x", false),
            ("def f(x):\n    return x.│\n", "x", false),
            ("for x in xs:\n    x.│\n", "x", false),
        ] {
            assert_eq!(bound(fixture, name), want, "{fixture} {name}");
        }
    }

    /// D6's adversarial inputs.
    #[test]
    fn site_def_path() {
        let f: &[&str] = &["f"];
        let fg: &[&str] = &["f", "g"];
        let top: &[&str] = &[];
        for (fixture, want) in [
            ("def f(l):\n    l.│\n", f),
            // Blank, whitespace-only and comment-only lines are skipped.
            ("def f(l):\n    x = 1\n\n    l.│\n", f),
            ("def f(l):\n    x = 1\n        \n    l.│\n", f),
            ("def f(l):\n    x = 1\n  \n    l.│\n", f),
            ("def f(l):\n    x = 1\n# c\n    l.│\n", f),
            ("def f(l):\n# c\n    l.│\n", f),
            // Lines that begin inside a bracket or after a `\`.
            ("def f(l):\n    x = foo(\n1,\n)\n    l.│\n", f),
            ("def f(l):\n    x = foo(\n1, l.│\n", f),
            ("def f(l):\n    x = foo(\n1,\n)\nl.│\n", top),
            // An unclosed bracket runs on until a statement keyword.
            ("def f(l):\n    x = foo(\n1,\n    │\n", f),
            ("def f(l):\n    x = foo(\ndef g(m):\n    m.│\n", &["g"]),
            ("def f(l):\n    x = foo(\n    return l\nl.│\n", top),
            ("x = foo(\ndef g(m):\n    m.│\n", &["g"]),
            ("def f(l):\n    x = 1 + \\\n2\n    l.│\n", f),
            ("def f(l):\n    x = 1 + \\\nl.│\n", f),
            // A `\` inside a comment continues nothing.
            ("def f(l):\n    x = 1 # \\\nl.│\n", top),
            // Lines that begin inside a string.
            ("def f(l):\n    \"\"\"\ndef g():\n\"\"\"\n    l.│\n", f),
            ("def f(l):\n    \"\"\"\ndef g():\n    l.│\n", f),
            ("\"def x(:\"\n    l.│\n", top),
            ("x = \"\"\"\ndef x(:\n\"\"\"\nl.│\n", top),
            // On the `def` line, before the header's `:`: the outer scope.
            ("def f(l: Lib = │\n", top),
            ("def f(l: Lib = l.│):\n    pass\n", top),
            ("def f(l = [\n    1,\n    l.│\n]):\n    pass\n", top),
            ("def f(l = [\n1,\nl.│\n]):\n    pass\n", top),
            ("│def f(l):\n    pass\n", top),
            ("def f│(l):\n    pass\n", top),
            ("def f(\n    l,\n    m):\n    l.│\n", f),
            ("def f(\nl,\n):\n    l.│\n", f),
            ("def f(l) -> str:\n    l.│\n", f),
            // After it, on the same line: the body.
            ("def f(l):│\n", f),
            ("def f(l): return l.│\n", f),
            // Nesting, and back out.
            ("def f(l):\n    def g(m):\n        m.│\n", fg),
            ("def f(l):\n    def g(m):\n        pass\n    l.│\n", f),
            ("def f(l):\n    def g(m):\n        pass\n\n    l.│\n", f),
            ("def f(l):\n    def g(m):\n        pass\nl.│\n", top),
            ("def f(l):\n    def g(): return l.│\n", fg),
            ("def f(l):\n    def g(m):│\n", fg),
            ("def f(l):\n    def g(m│):\n", f),
            (
                "def f(l):\n    if l:\n        def g(m):\n            m.│\n",
                fg,
            ),
            ("def f(l):\n    if l:\n        x = 1\n    l.│\n", f),
            // Under an `if`, in both branches, twice.
            ("if x:\n    def f(l):\n        l.│\n", f),
            (
                "if x:\n    def f(l):\n        pass\nelse:\n    def f(l):\n        l.│\n",
                f,
            ),
            ("if x:\n    def f(l):\n        pass\nelse:\n    l.│\n", top),
            ("def f(l):\n    pass\ndef f(l):\n    l.│\n", f),
            // The cursor's own indent decides on a blank line.
            ("def f(l):\n    x = 1\n│\n    y = 2\n", top),
            ("def f(l):\n    x = 1\n  │\n", f),
            ("def f(l):\n    x = 1\n    │\n", f),
            ("def f(l):\n    def g(m):\n        x = 1\n    │\n", f),
            ("def f(l):\n    x = 1\n    │y = 2\n", f),
            // EOF after a dedent.
            ("def f(l):\n    pass\n│", top),
            ("def f(l):\n    pass\n\n│", top),
            ("def f(l):\n    pass│", f),
            // A lambda is not a def.
            ("x = lambda l: l.│\n", top),
            ("def f(m):\n    g = lambda l: l.│\n", f),
            // A `def` with no name is not a def line.
            ("def (l):\n    l.│\n", top),
            ("│", top),
        ] {
            assert_eq!(scope(fixture), want, "{fixture:?}");
        }
    }

    #[test]
    fn site_def_path_byte_columns() {
        // A tab is one byte, as the lexer counts it: `g` is at 2, the
        // cursor's line at 1.
        assert_eq!(scope("def f(l):\n\tl.│\n"), ["f"]);
        assert_eq!(scope("def f(l):\n  def g(m):\n\tm.│\n"), ["f"]);
        assert_eq!(scope("def f(l):\n  def g(m):\n\t\t\tm.│\n"), ["f", "g"]);
        // CRLF is LF.
        assert_eq!(scope("def f(l):\r\n    x = 1\r\n\r\n    l.│\r\n"), ["f"]);
        assert_eq!(scope("def f(l):\r\n    pass\r\nl.│\r\n"), [] as [&str; 0]);
        assert_eq!(scope("def f(l):\r\n    x = 1 + \\\r\nl.│\r\n"), ["f"]);
        assert_eq!(scope("def f(l):\r\n    x = 1\r\n│\r\n"), [] as [&str; 0]);
        // A BOM is no indent.
        assert_eq!(scope("\u{feff}def f(l):\n    l.│\n"), ["f"]);
        assert_eq!(scope("\u{feff}def f(l):\n    pass\nl.│\n"), [] as [&str; 0]);
        // A form feed is no indent, and a line of one is blank.
        assert_eq!(scope("def f(l):\n\x0c    l.│\n"), ["f"]);
        assert_eq!(scope("def f(l):\n    x = 1\n\x0c\n    l.│\n"), ["f"]);
        assert_eq!(scope("\x0cdef f(l):\n    l.│\n"), ["f"]);
        // Multibyte text before the cursor: the client's UTF-16 column is
        // converted first, and the indent is counted in bytes.
        let text = "def f(l):\n    s = \"日本語 😀\"; l.up\nl.x\n";
        let before = "    s = \"日本語 😀\"; l.";
        let col = before.encode_utf16().count() as u32;
        let o =
            position::to_offset(text, Position::new(1, col), Encoding::Utf16).expect("in range");
        assert_eq!(text.get(o..o + 2), Some("up"));
        assert_eq!(scope_of(text, o), ["f"]);
        let o = position::to_offset(text, Position::new(2, 2), Encoding::Utf16).expect("in range");
        assert_eq!(scope_of(text, o), [] as [&str; 0]);
    }

    /// The defs of a parsed module, each with its path and the bytes its
    /// body covers: from the end of its header's `:` to the end of its last
    /// line.
    fn ast_defs(text: &str) -> Vec<(Vec<String>, usize, usize)> {
        use starlark::syntax::ast::{AstStmt, Stmt};
        fn walk(
            s: &AstStmt,
            text: &str,
            path: &mut Vec<String>,
            out: &mut Vec<(Vec<String>, usize, usize)>,
        ) {
            if let Stmt::Def(def) = &s.node {
                path.push(def.name.node.ident.clone());
                let header = def.signature_span().end().get() as usize;
                let colon = header
                    + cut(text, header..text.len())
                        .find(':')
                        .expect("header colon")
                    + 1;
                let last = (s.span.end().get() as usize).saturating_sub(1);
                out.push((path.clone(), colon, line_end(text, last)));
                walk(&def.body, text, path, out);
                path.pop();
                return;
            }
            s.visit_stmt(|c| walk(c, text, path, out));
        }
        let ast = starlark::syntax::AstModule::parse(
            "oracle.star",
            text.to_string(),
            &starlark::syntax::Dialect::Extended,
        )
        .expect("oracle fixture parses");
        let mut out = Vec::new();
        walk(ast.statement(), text, &mut Vec::new(), &mut out);
        out
    }

    fn cut(text: &str, r: Range<usize>) -> &str {
        text.get(r).expect("a char boundary")
    }

    /// The AST's answer at `pos`: the innermost def whose body covers it.
    fn ast_scope(defs: &[(Vec<String>, usize, usize)], pos: usize) -> Vec<String> {
        defs.iter()
            .filter(|(_, from, to)| *from <= pos && pos <= *to)
            .map(|(path, ..)| path.clone())
            .max_by_key(Vec::len)
            .unwrap_or_default()
    }

    const ORACLE_FIXTURE: &str = r#"load("//x:y.star", "Lib")

X = 1

def f(l, m = [
    1,
    2,
]):
    """Doc.
def g():
"""
    x = l.name
    if x:
        y = foo(
1,
            2,
        )
    z = 1 + \
2
# comment at col 0
    def g(a):
        return a.b

    def h(): return l.c
    for i in m:
        def n(b: Lib = Lib(
            name = "n",
        )) -> str:
            return b.name  # trailing
    return m

def k(
    a,
    b) -> str:
    return a

if X:
    def f2(l):
        pass
else:
    def f2(l):
        return l.x

lam = lambda l: l.y
d = {"a": [1, 2], "b": (3,)}
"#;

    /// D6's oracle: at every token boundary and every line end, the token
    /// rule agrees with the AST's def nesting; and again with the character
    /// before the cursor deleted (a document that no longer parses), except
    /// where that character is the one the answer turns on (a line break,
    /// a header's `:`).
    #[test]
    fn site_def_path_matches_ast() {
        let crlf = ORACLE_FIXTURE.replace('\n', "\r\n");
        let mut checked = 0usize;
        for text in [ORACLE_FIXTURE, crlf.as_str()] {
            let defs = ast_defs(text);
            assert_eq!(defs.len(), 7, "{defs:?}");
            let colons: HashSet<usize> = defs.iter().map(|(_, from, _)| *from).collect();
            let tokens = lex(text);
            let mut samples = std::collections::BTreeSet::new();
            samples.insert(text.len());
            for t in tokens.toks.iter().filter(|t| t.kind != Kind::Comment) {
                samples.insert(t.start as usize);
                samples.insert(t.end as usize);
                // The line end of every line with code on it.
                samples.insert(line_end(text, t.start as usize));
            }
            for &pos in &samples {
                let want = ast_scope(&defs, pos);
                assert_eq!(
                    scope_of(text, pos),
                    want,
                    "at {pos}: {:?}│{:?}",
                    cut(text, line_start(text, pos)..pos),
                    cut(text, pos..line_end(text, pos))
                );
                checked += 1;
                let Some(c) = cut(text, 0..pos).chars().next_back() else {
                    continue;
                };
                if matches!(c, '\n' | '\r') || colons.contains(&pos) {
                    continue;
                }
                let at = pos - c.len_utf8();
                let broken = format!("{}{}", cut(text, 0..at), cut(text, pos..text.len()));
                assert_eq!(
                    scope_of(&broken, at),
                    want,
                    "{c:?} deleted at {pos}: {:?}│{:?}",
                    cut(&broken, line_start(&broken, at)..at),
                    cut(&broken, at..line_end(&broken, at))
                );
            }
        }
        assert!(checked > 300, "sampled only {checked} positions");
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
        // A field's string value is a site; the handlers decide whether heph
        // has anything there (only a field's string literals).
        assert_eq!(kind(&at("target(name = \"hel│lo\")\n")), "field_value");
        assert_eq!(kind(&at("other(name = \"hel│lo\")\n")), "none");
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
        assert_eq!(
            kind(&r),
            "arg_key",
            "the inner call is the site, not target"
        );
        match r.site {
            Site::ArgKey { call, prefix, .. } => {
                assert_eq!(call.callee, "f");
                assert_eq!(prefix, "dri");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn site_arg_key_in_any_plain_call() {
        let r = at("my_rule(name = \"a\", out = 1, sr│\n\nbroken = (\n");
        let Site::ArgKey { call, edit, prefix } = r.site else {
            panic!("{:?}", r.site);
        };
        assert_eq!(call.callee, "my_rule");
        assert_eq!(call.keys, vec!["name".to_string(), "out".to_string()]);
        assert_eq!((edit.end - edit.start, prefix.as_str()), (2, "sr"));
        assert!(
            r.degraded
                .iter()
                .any(|d| matches!(d, Degraded::CallExtentGuessed { .. })),
            "{:?}",
            r.degraded
        );
        assert_eq!(kind(&at("f(│)\n")), "arg_key", "right after the `(`");
        assert_eq!(kind(&at("f(x, │)\n")), "arg_key", "after a comma");
        assert_eq!(kind(&at("f(\n    a = 1,\n    │\n)\n")), "arg_key");
    }

    #[test]
    fn site_arg_key_is_not_every_bracket() {
        for fixture in [
            "def f(a, │):\n    pass\n", // the def's own parameters
            "x.f(│)\n",                 // a member call
            "f(a = │)\n",               // a value position
            "f(a = M│)\n",              // a value position, mid-word
            "f(**│)\n",                 // after `**`
            "f(x + │)\n",               // inside an expression
            "f([a, │])\n",              // inside a list argument
            "x = (a, │)\n",             // a tuple
        ] {
            assert_eq!(kind(&at(fixture)), "none", "{fixture:?}");
        }
        // A lex error after the cursor does not matter.
        assert_eq!(kind(&at("f(\"a\", │\n\"\"\"\n")), "arg_key");
        // heph's builtins keep their own sites.
        assert_eq!(kind(&at("target(│)\n")), "target_key");
        assert_eq!(kind(&at("provider_state(│)\n")), "state_key");
        assert_eq!(kind(&at("load(\"//x\", │)\n")), "none");
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
        // Every site inside the call is shadowed, a field value included.
        for src in ["def target(**kw):\n    pass\n", "target = None\n"] {
            for call in [
                "target(dri│)\n",
                "target(name = \"x\", workdir = \"tr│\")\n",
            ] {
                let r = with_overlay(&format!("{src}{call}"), &["target"]);
                assert_eq!(kind(&r), "none", "{call}");
                assert_eq!(
                    r.reason,
                    Some(Reason::CalleeRebound {
                        name: "target".to_string()
                    }),
                    "{call}"
                );
            }
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
    fn corpora() -> [(&'static str, String); 3] {
        let mut clean = String::new();
        let mut adversarial = String::new();
        // Every line an unclosed call: the bracket stack prunes each by its
        // guessed extent, which must stay linear.
        let mut unclosed = String::new();
        for _ in 0..10_000 {
            unclosed.push_str("x = foo(1,\n");
        }
        unclosed.push_str("target(name = \"last\", dri");
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
        [
            ("clean", clean),
            ("adversarial", adversarial),
            ("unclosed", unclosed),
        ]
    }

    /// The fastest resolution over a few runs, the cursor at the end (the
    /// resolver walks every token before it).
    ///
    /// The fastest, not the slowest: a regression slows every run, while a
    /// preempted thread on a loaded CI runner slows only some. Taking the
    /// slowest made the bound a measure of the runner — a 6ms resolution
    /// read as 95ms on `Test darwin/arm64`.
    fn time_resolve(text: &str) -> (Duration, Resolution) {
        let tokens = lex(text);
        let doc = Doc {
            text,
            tokens: &tokens,
            overlay: None,
            ast: AstState::Current,
        };
        let mut best = Duration::MAX;
        let mut last = None;
        for _ in 0..5 {
            let started = Instant::now();
            let r = resolve(&doc, text.len());
            best = best.min(started.elapsed());
            last = Some(r);
        }
        (best, last.unwrap())
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

    /// A 100,000-element list before the `.`: the display is found by one
    /// bracket-link hop, not a walk over its elements. The cursor sits in a
    /// def, so the def path is walked too. The bound is a gross-regression
    /// guard, generous for a loaded CI runner: a walk over the elements
    /// would take seconds, not this.
    #[test]
    fn literal_receiver_on_a_100k_element_list_under_1s() {
        let mut text = String::from("def f():\n    return [");
        for i in 0..100_000 {
            text.push_str(&format!("{i}, "));
        }
        text.push_str("].");
        let (took, r) = time_resolve(&text);
        match r.site {
            Site::Member {
                receiver, scope, ..
            } => {
                assert_eq!(
                    receiver,
                    Receiver::Literal {
                        kind: Literal::List
                    }
                );
                assert_eq!(scope, ["f"]);
            }
            other => panic!("not a member site: {other:?}"),
        }
        assert!(took < Duration::from_secs(1), "resolving took {took:?}");
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
