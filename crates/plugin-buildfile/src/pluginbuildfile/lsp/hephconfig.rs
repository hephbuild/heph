//! `.hephconfig` documents: completion, hover and diagnostics.
//!
//! The server routes a document here by file name
//! ([`hconfig::is_config_file_name`]): the base `.hephconfig`, the legacy
//! `.hephconfig2`, and every `<profile>.hephconfig`. None of it touches the
//! engine or the Starlark analysis.
//!
//! Two sources, each the authority on one question:
//!
//! - **What loads** is the parser's: every edit runs the same
//!   `serde_yaml::from_str::<ConfigYaml>` the loader does and publishes its
//!   error, at its location. A file with no parser error loads (profile merging
//!   aside, which needs the other files).
//! - **What could go here** is the schema's ([`hconfig::config_json_schema`],
//!   derived from the parser's own types): the keys and values completion
//!   offers, hover's text, and the unknown-key check, which flags *every*
//!   unknown key where the parser stops at the first.
//!
//! Where the cursor is comes from [`Scan`], a line-based reading of YAML's
//! block structure (indentation, `- ` items, `key:`), so it answers on a
//! document that does not parse, which is most of the time someone is typing.
//! What it cannot read it leaves opaque rather than guess at: flow collections
//! (`{a: 1}`, `[a]`) and quoted scalars, including the lines they continue
//! onto; block scalars (`|`, `>`); a key behind a tag, an anchor or `?`; a line
//! indented with a tab; and a key out of line with its siblings. Nothing under
//! an opaque line is completed or checked, so the parser's error, when there
//! is one, is the only word on it.

use std::sync::LazyLock;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Diagnostic, DiagnosticSeverity,
    Documentation, Hover, HoverContents, MarkupContent, MarkupKind, Range, TextEdit,
};
use serde_json::Value;

use super::position::{self, Encoding};

/// Built once per server: the schema never changes under a running binary.
static SCHEMA: LazyLock<Value> = LazyLock::new(hconfig::config_json_schema);

/// A plugin entry's source keys: an entry sets exactly one.
const PLUGIN_SOURCES: [&str; 3] = ["builtin", "path", "url"];

/// One step of a key path: a mapping key, a sequence item, or a line the
/// scanner could not read, beneath which nothing is the schema's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    Key(String),
    Item,
    Opaque,
}

/// An open mapping key or sequence item, while scanning.
#[derive(Debug)]
struct Frame {
    /// The column of the key, or of the `-`.
    indent: usize,
    seg: Seg,
    /// A key with nothing after its `:`, which a `-` at its own column
    /// continues (`plugins:` then `- builtin: x`).
    open: bool,
    /// Tells two items of one sequence apart, so their keys are not siblings.
    id: usize,
    /// The column of its first child key, which every later one must share.
    child: Option<usize>,
}

/// One `key:` of the document, with where it sits.
#[derive(Debug)]
struct KeyEntry {
    line_start: usize,
    /// Byte span of the key (quotes included), absolute.
    start: usize,
    end: usize,
    name: String,
    parent_path: Vec<Seg>,
    parent_ids: Vec<usize>,
}

/// One line's block structure.
#[derive(Debug, Default)]
struct Shape<'a> {
    indent: usize,
    /// The indentation holds a tab, which YAML refuses: the line is not read.
    tab: bool,
    /// Column of each `- ` before the content.
    dashes: Vec<usize>,
    /// Column where the content after indentation and dashes starts.
    content: usize,
    key: Option<KeyTok<'a>>,
    /// Column and text of the value after `key:`, comment stripped.
    value: Option<(usize, &'a str)>,
}

#[derive(Debug)]
struct KeyTok<'a> {
    start: usize,
    end: usize,
    colon: usize,
    name: &'a str,
}

fn shape(line: &str) -> Shape<'_> {
    let bytes = line.as_bytes();
    let indent = bytes
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t'))
        .count();
    let tab = bytes.get(..indent).is_some_and(|l| l.contains(&b'\t'));
    let mut i = indent;
    let mut dashes = Vec::new();
    while bytes.get(i) == Some(&b'-') && matches!(bytes.get(i + 1), None | Some(b' ' | b'\t')) {
        dashes.push(i);
        i += 1;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
    }
    let content = i;
    let key = if tab { None } else { key_at(line, content) };
    let value = key.as_ref().and_then(|k| {
        let mut v = k.colon + 1;
        while matches!(bytes.get(v), Some(b' ' | b'\t')) {
            v += 1;
        }
        let text = strip_comment(line.get(v..)?).trim_end();
        (!text.is_empty()).then_some((v, text))
    });
    Shape {
        indent,
        tab,
        dashes,
        content,
        key,
        value,
    }
}

/// The `key:` starting at column `at`, if one does: a plain key ends at the
/// first `:` followed by a blank or the line's end; a quoted one at its
/// closing quote, which a `:` must follow. A key behind a tag, an anchor or a
/// `?`, and a `:` with no key before it, are not read.
fn key_at(line: &str, at: usize) -> Option<KeyTok<'_>> {
    let bytes = line.as_bytes();
    let first = *bytes.get(at)?;
    let colon_ok = |j: usize| {
        bytes.get(j) == Some(&b':') && matches!(bytes.get(j + 1), None | Some(b' ' | b'\t'))
    };
    if first == b'"' || first == b'\'' {
        let close = at + 1 + bytes.get(at + 1..)?.iter().position(|b| *b == first)?;
        let mut colon = close + 1;
        while bytes.get(colon) == Some(&b' ') {
            colon += 1;
        }
        return colon_ok(colon).then(|| KeyTok {
            start: at,
            end: close + 1,
            colon,
            name: line.get(at + 1..close).unwrap_or_default(),
        });
    }
    if matches!(
        first,
        b'#' | b'{' | b'[' | b'&' | b'*' | b'!' | b'|' | b'>' | b'?'
    ) {
        return None;
    }
    let mut j = at;
    while let Some(&b) = bytes.get(j) {
        if colon_ok(j) {
            let name = line.get(at..j)?.trim_end();
            return (!name.is_empty()).then_some(KeyTok {
                start: at,
                end: at + name.len(),
                colon: j,
                name,
            });
        }
        if b == b'#' && j > at && matches!(bytes.get(j - 1), Some(b' ' | b'\t')) {
            return None;
        }
        j += 1;
    }
    None
}

/// What a line leaves open for the lines after it: flow-collection brackets,
/// a quoted scalar, and whether a plain scalar is still running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Flow {
    depth: usize,
    quote: Option<u8>,
    plain: bool,
}

impl Flow {
    fn open(&self) -> bool {
        self.depth > 0 || self.quote.is_some()
    }
}

/// Read `text` on from `flow`: the state at its end, and where a comment
/// starts, if one does. A quote or a bracket opens something only where a
/// scalar could start (`it's` and `a[0]` are plain text); a `#` is a comment
/// only outside quotes and after a blank.
fn read_flow(text: &str, mut flow: Flow) -> (Flow, Option<usize>) {
    let bytes = text.as_bytes();
    let mut escaped = false;
    let mut prev_blank = true;
    for (i, &b) in bytes.iter().enumerate() {
        let blank = b == b' ' || b == b'\t';
        match flow.quote {
            Some(b'"') => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    flow.quote = None;
                }
            }
            Some(q) => {
                if b == q {
                    flow.quote = None;
                }
            }
            None if b == b'#' && prev_blank => return (flow, Some(i)),
            None if blank => {}
            None if flow.plain => {
                // A plain scalar runs on to a flow indicator (inside a
                // collection) or to `: `.
                if flow.depth > 0 && matches!(b, b',' | b']' | b'}') {
                    flow.plain = false;
                    if b != b',' {
                        flow.depth = flow.depth.saturating_sub(1);
                    }
                } else if b == b':' && matches!(bytes.get(i + 1), None | Some(b' ' | b'\t')) {
                    flow.plain = false;
                }
            }
            None => match b {
                b'"' | b'\'' => flow.quote = Some(b),
                b'[' | b'{' => flow.depth += 1,
                b']' | b'}' => flow.depth = flow.depth.saturating_sub(1),
                b',' | b':' | b'?' => {}
                _ => flow.plain = true,
            },
        }
        prev_blank = blank;
    }
    (flow, None)
}

/// `text` up to a ` #` comment that is not inside quotes.
fn strip_comment(text: &str) -> &str {
    match read_flow(text, Flow::default()).1 {
        Some(i) => text.get(..i).unwrap_or(text),
        None => text,
    }
}

/// The block structure above a position: the stack of open keys and items.
#[derive(Default)]
struct Scan {
    stack: Vec<Frame>,
    /// The column of the first top-level key.
    root_child: Option<usize>,
    /// Inside a block scalar opened by a key at this column: lines deeper than
    /// it are text.
    block_scalar: Option<usize>,
    /// Inside a flow collection or quoted scalar an earlier line opened: lines
    /// are opaque until it closes.
    cont: Option<Flow>,
    next_id: usize,
}

impl Scan {
    fn push(&mut self, indent: usize, seg: Seg, open: bool) {
        self.next_id += 1;
        self.stack.push(Frame {
            indent,
            seg,
            open,
            id: self.next_id,
            child: None,
        });
    }

    /// A `-` at column `d` closes everything deeper and the previous item at
    /// `d`, but continues an open key at `d` (`plugins:` / `- …`).
    fn pop_for_dash(&mut self, d: usize) {
        while let Some(top) = self.stack.last() {
            let continues = top.indent == d && top.open && matches!(top.seg, Seg::Key(_));
            if top.indent > d || (top.indent == d && !continues) {
                self.stack.pop();
            } else {
                break;
            }
        }
    }

    /// A key at column `k` closes everything at `k` or deeper.
    fn pop_for_key(&mut self, k: usize) {
        while self.stack.last().is_some_and(|top| top.indent >= k) {
            self.stack.pop();
        }
    }

    /// Whether a key at column `k` lines up with its siblings: a mapping's
    /// first key sets the column, and YAML refuses a later one elsewhere.
    fn aligned(&mut self, k: usize) -> bool {
        let slot = match self.stack.last_mut() {
            Some(top) => &mut top.child,
            None => &mut self.root_child,
        };
        match *slot {
            Some(c) => c == k,
            None => {
                *slot = Some(k);
                true
            }
        }
    }

    fn path(&self) -> Vec<Seg> {
        self.stack.iter().map(|f| f.seg.clone()).collect()
    }

    fn ids(&self) -> Vec<usize> {
        self.stack.iter().map(|f| f.id).collect()
    }

    /// Whether a line is block-scalar text: blank, or deeper than the key that
    /// opened the scalar (`indent`: the line's, or a blank line's cursor
    /// column). Closes the scalar at the first line that is not.
    fn in_block_scalar(&mut self, indent: usize, blank: bool) -> bool {
        match self.block_scalar {
            Some(k) if blank || indent > k => true,
            Some(_) => {
                self.block_scalar = None;
                false
            }
            None => false,
        }
    }

    /// Feed one whole line; the key it declares, if any.
    fn feed(&mut self, line_start: usize, line: &str) -> Option<KeyEntry> {
        if let Some(open) = self.cont {
            let (flow, _) = read_flow(line, open);
            self.cont = flow.open().then_some(flow);
            return None;
        }
        let s = shape(line);
        let rest = line.get(s.content..).unwrap_or_default();
        let empty = rest.is_empty() || rest.starts_with('#');
        let blank = s.dashes.is_empty() && empty;
        if self.in_block_scalar(s.indent, blank) || blank {
            return None;
        }
        let (flow, _) = read_flow(rest, Flow::default());
        self.cont = flow.open().then_some(flow);
        for &d in &s.dashes {
            self.pop_for_dash(d);
            self.push(d, Seg::Item, true);
        }
        let Some(key) = s.key else {
            // Content the scanner cannot read as a key: whatever sits under
            // it is not the schema's to judge.
            if !empty {
                self.pop_for_key(s.content);
                self.push(s.content, Seg::Opaque, false);
            }
            return None;
        };
        self.pop_for_key(key.start);
        if !self.aligned(key.start) {
            self.push(key.start, Seg::Opaque, false);
            return None;
        }
        let entry = KeyEntry {
            line_start,
            start: line_start + key.start,
            end: line_start + key.end,
            name: key.name.to_string(),
            parent_path: self.path(),
            parent_ids: self.ids(),
        };
        if s.value
            .is_some_and(|(_, v)| v.starts_with('|') || v.starts_with('>'))
        {
            self.block_scalar = Some(key.start);
        }
        self.push(key.start, Seg::Key(entry.name.clone()), s.value.is_none());
        Some(entry)
    }
}

/// A leading byte-order mark's length: the text starts after it.
fn bom(text: &str) -> usize {
    if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    }
}

/// Each line with its absolute start, without its line break.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let skip = bom(text);
    let mut start = skip;
    text.get(skip..)
        .unwrap_or_default()
        .split_inclusive('\n')
        .map(move |l| {
            let s = start;
            start += l.len();
            (s, l.trim_end_matches(['\n', '\r']))
        })
}

/// Every key of the document, in order.
fn keys(text: &str) -> Vec<KeyEntry> {
    let mut scan = Scan::default();
    lines(text)
        .filter_map(|(start, line)| scan.feed(start, line))
        .collect()
}

/// The line holding `offset`: its start and its text.
fn line_at(text: &str, offset: usize) -> Option<(usize, &str)> {
    let before = text.get(..offset)?;
    let start = before.rfind('\n').map_or_else(|| bom(text), |i| i + 1);
    let rest = text.get(start..)?;
    let end = rest.find('\n').unwrap_or(rest.len());
    Some((start, rest.get(..end)?.trim_end_matches('\r')))
}

/// The block structure above the line starting at `line_start`.
fn scan_to(text: &str, line_start: usize) -> Option<Scan> {
    let mut scan = Scan::default();
    for (start, l) in lines(text.get(..line_start)?) {
        scan.feed(start, l);
    }
    Some(scan)
}

/// What the cursor is on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Cursor {
    /// A key position: a key being written, or where one may start.
    Key {
        parent_path: Vec<Seg>,
        parent_ids: Vec<usize>,
        prefix: String,
        /// Byte span to replace, absolute.
        start: usize,
        end: usize,
        /// The line already has `key:`; completing replaces the name only.
        has_colon: bool,
    },
    /// After `key: `.
    Value {
        path: Vec<Seg>,
        prefix: String,
        start: usize,
        end: usize,
    },
}

fn is_key_char(b: u8) -> bool {
    !matches!(
        b,
        b':' | b' ' | b'\t' | b'#' | b'{' | b'}' | b'[' | b']' | b',' | b'"' | b'\''
    )
}

/// Classify `offset`. `None` inside a block scalar, a flow collection, a
/// quoted scalar, a comment, a tab-indented line, or after a scalar list item:
/// no key or value of the schema's.
pub(crate) fn cursor(text: &str, offset: usize) -> Option<Cursor> {
    let (line_start, line) = line_at(text, offset)?;
    let col = offset.checked_sub(line_start)?;
    let mut scan = scan_to(text, line_start)?;
    if scan.cont.is_some() {
        return None;
    }
    let full = shape(line);
    let blank =
        full.dashes.is_empty() && matches!(line.as_bytes().get(full.content), None | Some(b'#'));
    // On a blank line the cursor's column is what a typed character's
    // indentation would be.
    if scan.in_block_scalar(if blank { col } else { full.indent }, false) || full.tab {
        return None;
    }
    let bytes = line.as_bytes();
    let run_end = |from: usize| {
        from + bytes
            .get(from..)
            .map_or(0, |r| r.iter().take_while(|b| is_key_char(**b)).count())
    };

    // After `key:` on this line: a value. Right after the colon, with no
    // blank yet, a value would glue onto it (`backend:fs`).
    if let Some(key) = full.key.as_ref().filter(|k| col > k.colon) {
        if !matches!(bytes.get(key.colon + 1), Some(b' ' | b'\t')) {
            return None;
        }
        for &d in &full.dashes {
            scan.pop_for_dash(d);
            scan.push(d, Seg::Item, true);
        }
        scan.pop_for_key(key.start);
        let mut path = scan.path();
        path.push(Seg::Key(key.name.to_string()));
        let mut vstart = key.colon + 1;
        while matches!(bytes.get(vstart), Some(b' ' | b'\t')) && vstart < col {
            vstart += 1;
        }
        match bytes.get(vstart) {
            Some(b'"' | b'\'') => vstart += 1,
            Some(b'{' | b'[' | b'|' | b'>' | b'#') if vstart < col => return None,
            _ => {}
        }
        if col < vstart {
            return None;
        }
        let prefix = line.get(vstart..col)?;
        if !prefix.bytes().all(is_key_char) {
            return None;
        }
        return Some(Cursor::Value {
            path,
            prefix: prefix.to_string(),
            start: line_start + vstart,
            end: line_start + run_end(vstart).max(col),
        });
    }

    // Otherwise a key position, at the content column of the text before
    // the cursor.
    let before = shape(line.get(..col)?);
    for &d in &before.dashes {
        scan.pop_for_dash(d);
        scan.push(d, Seg::Item, true);
    }
    let at = before.content.min(col);
    let prefix = line.get(at..col)?;
    if !prefix.bytes().all(is_key_char) {
        return None;
    }
    scan.pop_for_key(at);
    Some(Cursor::Key {
        parent_path: scan.path(),
        parent_ids: scan.ids(),
        prefix: prefix.to_string(),
        start: line_start + at,
        end: line_start + run_end(at).max(col),
        has_colon: full.key.is_some(),
    })
}

/// `--explain` for a config document: the cursor's classification, as one
/// JSON object. `offset` is `None` for a position past the end.
pub(crate) fn explain(text: &str, offset: Option<usize>) -> Value {
    let (kind, path, prefix) = match offset.and_then(|o| cursor(text, o)) {
        Some(Cursor::Key {
            parent_path,
            prefix,
            ..
        }) => ("key", Some(parent_path), Some(prefix)),
        Some(Cursor::Value { path, prefix, .. }) => ("value", Some(path), Some(prefix)),
        None => ("none", None, None),
    };
    serde_json::json!({
        "document": "hephconfig",
        "kind": kind,
        "path": path.as_deref().map(display_path),
        "in_schema": path.as_deref().is_some_and(|p| node_at(p).is_some()),
        "prefix": prefix,
    })
}

/// The schema node at `path`, from the root.
fn node_at<'a>(path: &[Seg]) -> Option<&'a Value> {
    path.iter().try_fold(&*SCHEMA, |node, seg| child(node, seg))
}

/// `v` with a `$ref` followed.
fn deref(v: &Value) -> &Value {
    match v.get("$ref").and_then(Value::as_str) {
        Some(r) => r
            .strip_prefix("#/$defs/")
            .and_then(|name| SCHEMA.get("$defs")?.get(name))
            .unwrap_or(v),
        None => v,
    }
}

/// `v` and every alternative it allows (`anyOf`/`oneOf`), refs followed and
/// `null` left out. Each node is visited once, so a `$ref` cycle ends.
fn branches(v: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    let mut seen: Vec<&Value> = Vec::new();
    let mut todo = vec![v];
    while let Some(v) = todo.pop() {
        let v = deref(v);
        if seen.iter().any(|s| std::ptr::eq(*s, v)) {
            continue;
        }
        seen.push(v);
        if v.get("type").and_then(Value::as_str) == Some("null") {
            continue;
        }
        let alts: Vec<&Value> = ["anyOf", "oneOf"]
            .iter()
            .filter_map(|k| v.get(*k).and_then(Value::as_array))
            .flatten()
            .collect();
        let any = !alts.is_empty();
        // Reversed onto the stack, so alternatives come out in schema order.
        todo.extend(alts.into_iter().rev());
        if !any || v.get("properties").is_some() || v.get("type").is_some() {
            out.push(v);
        }
    }
    out
}

fn child<'a>(node: &'a Value, seg: &Seg) -> Option<&'a Value> {
    branches(node).into_iter().find_map(|b| match seg {
        Seg::Key(k) => b
            .get("properties")
            .and_then(|p| p.get(k))
            .or_else(|| b.get("additionalProperties").filter(|a| a.is_object())),
        Seg::Item => b.get("items"),
        Seg::Opaque => None,
    })
}

/// The keys `node` declares, with their schemas, in schema order.
fn properties(node: &Value) -> Vec<(&str, &Value)> {
    let mut out: Vec<(&str, &Value)> = Vec::new();
    for b in branches(node) {
        for (k, v) in b
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if !out.iter().any(|(have, _)| *have == k.as_str()) {
                out.push((k.as_str(), v));
            }
        }
    }
    out
}

/// Whether `node` is a mapping that refuses keys it does not declare.
fn closed(node: &Value) -> bool {
    let objects: Vec<&Value> = branches(node)
        .into_iter()
        .filter(|b| b.get("properties").is_some() || b.get("additionalProperties").is_some())
        .collect();
    !objects.is_empty()
        && objects
            .iter()
            .all(|b| b.get("additionalProperties") == Some(&Value::Bool(false)))
}

/// Whether `b` allows `ty`: `"type": "<ty>"`, or a list holding it (how
/// schemars writes an `Option`, `["boolean", "null"]`).
fn allows_type(b: &Value, ty: &str) -> bool {
    match b.get("type") {
        Some(Value::String(s)) => s == ty,
        Some(Value::Array(a)) => a.iter().any(|t| t.as_str() == Some(ty)),
        _ => false,
    }
}

/// The literal values `node` allows, with their descriptions.
fn values(node: &Value) -> Vec<(String, Option<&str>)> {
    fn add<'a>(out: &mut Vec<(String, Option<&'a str>)>, v: String, d: Option<&'a str>) {
        if !out.iter().any(|(have, _)| *have == v) {
            out.push((v, d));
        }
    }
    let mut out = Vec::new();
    for b in branches(node) {
        let desc = b.get("description").and_then(Value::as_str);
        if let Some(c) = b.get("const").and_then(scalar_text) {
            add(&mut out, c, desc);
        }
        for e in b
            .get("enum")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(e) = scalar_text(e) {
                add(&mut out, e, None);
            }
        }
        if allows_type(b, "boolean") {
            add(&mut out, "true".to_string(), None);
            add(&mut out, "false".to_string(), None);
        }
    }
    out
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A short type, for an item's detail and hover's heading.
fn type_summary(node: &Value) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for b in branches(node) {
        let ty = if b.get("const").is_some() || b.get("enum").is_some() {
            Some("enum")
        } else if b.get("items").is_some() {
            Some("list")
        } else if b.get("properties").is_some() {
            Some("object")
        } else if b.get("additionalProperties").is_some_and(Value::is_object) {
            Some("map")
        } else {
            None
        };
        let named = ty.into_iter().chain(match b.get("type") {
            Some(Value::String(s)) if ty.is_none() => vec![s.as_str()],
            Some(Value::Array(a)) if ty.is_none() => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        });
        for t in named.filter(|t| *t != "null") {
            if !parts.contains(&t) {
                parts.push(t);
            }
        }
    }
    parts.join(" | ")
}

fn description(node: &Value) -> Option<String> {
    node.get("description")
        .and_then(Value::as_str)
        .or_else(|| {
            branches(node)
                .into_iter()
                .find_map(|b| b.get("description").and_then(Value::as_str))
        })
        .map(clean_doc)
}

/// A doc comment as Markdown for an editor: a rustdoc intra-doc link
/// (`` [`X`] ``, `` [`x`](Self::x) ``, `[text](crate::X)`) becomes its text,
/// a URL stays a link, and everything else, inline code and plain brackets
/// (`a[0]`) included, is left as written.
fn clean_doc(doc: &str) -> String {
    let mut out = String::with_capacity(doc.len());
    let mut rest = doc;
    while let Some(i) = rest.find(['`', '[']) {
        out.push_str(rest.get(..i).unwrap_or_default());
        let tail = rest.get(i..).unwrap_or_default();
        if tail.starts_with('`') {
            // Inline code, verbatim through its closing backtick.
            let code_len = tail
                .get(1..)
                .and_then(|t| t.find('`'))
                .map_or(tail.len(), |j| j + 2);
            out.push_str(tail.get(..code_len).unwrap_or(tail));
            rest = tail.get(code_len..).unwrap_or_default();
            continue;
        }
        match intra_doc_link(tail) {
            Some((text, len)) => {
                out.push_str(text);
                rest = tail.get(len..).unwrap_or_default();
            }
            None => {
                out.push('[');
                rest = tail.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out
}

/// At a `[`: the text an intra-doc link renders as and the link's length in
/// `tail`, or `None` when the bracket is not one (prose, or a URL link).
fn intra_doc_link(tail: &str) -> Option<(&str, usize)> {
    let close = tail.find(']')?;
    let inner = tail.get(1..close)?;
    let after = tail.get(close + 1..).unwrap_or_default();
    if let Some(target) = after.strip_prefix('(') {
        let end = target.find(')')?;
        let url = target.get(..end)?;
        if url.is_empty() || url.contains(char::is_whitespace) {
            return None;
        }
        if url.starts_with("http://") || url.starts_with("https://") {
            return None;
        }
        // `[text](target)`: the text, the target dropped.
        return Some((inner, close + 2 + end + 1));
    }
    // `` [`X`] ``: the code span alone.
    let code = inner.len() >= 3
        && inner.starts_with('`')
        && inner.ends_with('`')
        && inner.matches('`').count() == 2;
    code.then_some((inner, close + 1))
}

fn display_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        let name = match seg {
            Seg::Key(k) => k.as_str(),
            Seg::Opaque => "?",
            Seg::Item => {
                out.push_str("[]");
                continue;
            }
        };
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(name);
    }
    out
}

fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

fn range(text: &str, start: usize, end: usize, enc: Encoding) -> Range {
    Range::new(
        position::to_position(text, start, enc),
        position::to_position(text, end, enc),
    )
}

/// The completion items at `offset`.
pub(crate) fn completion(text: &str, offset: usize, enc: Encoding) -> Vec<CompletionItem> {
    let Some(cur) = cursor(text, offset) else {
        return Vec::new();
    };
    match cur {
        Cursor::Key {
            parent_path,
            parent_ids,
            prefix,
            start,
            end,
            has_colon,
        } => {
            let Some(parent) = node_at(&parent_path) else {
                return Vec::new();
            };
            let (line_start, _) = line_at(text, offset).unwrap_or_default();
            let present: Vec<String> = keys(text)
                .into_iter()
                .filter(|k| k.parent_ids == parent_ids && k.line_start != line_start)
                .map(|k| k.name)
                .collect();
            // A plugin entry sets exactly one source: once it has one, the
            // other two are not offered.
            let has_source = display_path(&parent_path) == "plugins[]"
                && present.iter().any(|p| PLUGIN_SOURCES.contains(&p.as_str()));
            // Right after a `-`, the key needs a blank before it.
            let gap = if start > 0 && text.as_bytes().get(start - 1) == Some(&b'-') {
                " "
            } else {
                ""
            };
            properties(parent)
                .into_iter()
                .filter(|(k, _)| {
                    k.starts_with(prefix.as_str())
                        && !present.iter().any(|p| p == k)
                        && !(has_source && PLUGIN_SOURCES.contains(k))
                })
                .map(|(k, schema)| {
                    let ty = type_summary(schema);
                    let nested = ["object", "list", "map"].contains(&ty.as_str());
                    let new_text = match (has_colon, nested) {
                        (true, _) => format!("{gap}{k}"),
                        (false, true) => format!("{gap}{k}:"),
                        (false, false) => format!("{gap}{k}: "),
                    };
                    CompletionItem {
                        label: k.to_string(),
                        kind: Some(CompletionItemKind::PROPERTY),
                        detail: (!ty.is_empty()).then_some(ty),
                        documentation: description(schema).map(markdown),
                        filter_text: Some(k.to_string()),
                        sort_text: Some(format!("0_{k}")),
                        text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                            range(text, start, end, enc),
                            new_text,
                        ))),
                        ..Default::default()
                    }
                })
                .collect()
        }
        Cursor::Value {
            path,
            prefix,
            start,
            end,
        } => {
            let Some(node) = node_at(&path) else {
                return Vec::new();
            };
            values(node)
                .into_iter()
                .filter(|(v, _)| v.starts_with(prefix.as_str()))
                .map(|(v, desc)| CompletionItem {
                    label: v.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    documentation: desc.map(|d| markdown(clean_doc(d))),
                    filter_text: Some(v.clone()),
                    sort_text: Some(format!("0_{v}")),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                        range(text, start, end, enc),
                        v,
                    ))),
                    ..Default::default()
                })
                .collect()
        }
    }
}

/// The hover at `offset`: the key on its line, from the key to the line's end.
pub(crate) fn hover(text: &str, offset: usize, enc: Encoding) -> Option<Hover> {
    let (line_start, line) = line_at(text, offset)?;
    let mut scan = scan_to(text, line_start)?;
    let entry = scan.feed(line_start, line).filter(|e| offset >= e.start)?;
    let (start, end) = (entry.start, entry.end);
    let mut path = entry.parent_path;
    path.push(Seg::Key(entry.name));
    let node = node_at(&path)?;
    let mut md = format!("`{}`", display_path(&path));
    let ty = type_summary(node);
    if !ty.is_empty() {
        md.push_str(&format!(": {ty}"));
    }
    if let Some(d) = description(node) {
        md.push_str("\n\n");
        md.push_str(&d);
    }
    let vals = values(node);
    if !vals.is_empty() && !vals.iter().all(|(v, _)| v == "true" || v == "false") {
        let list: Vec<String> = vals.iter().map(|(v, _)| format!("`{v}`")).collect();
        md.push_str(&format!("\n\nOne of {}.", list.join(", ")));
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: Some(range(text, start, end, enc)),
    })
}

/// Every key the schema does not know, then the parser's error if the
/// document does not load and that error is not one of those keys again.
pub(crate) fn diagnostics(text: &str, enc: Encoding) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    // Each flagged key: its start, and its name.
    let mut flagged: Vec<(usize, String)> = Vec::new();
    for k in keys(text) {
        let Some(parent) = node_at(&k.parent_path) else {
            continue;
        };
        if !closed(parent) {
            continue;
        }
        let known = properties(parent);
        if known.iter().any(|(name, _)| *name == k.name) {
            continue;
        }
        let names: Vec<&str> = known.iter().map(|(n, _)| *n).collect();
        let place = match display_path(&k.parent_path) {
            p if p.is_empty() => "at the top level".to_string(),
            p => format!("in `{p}`"),
        };
        let mut message = format!("unknown key `{}` {place}", k.name);
        if let Some(near) = names
            .iter()
            .map(|n| (strsim::jaro_winkler(n, &k.name), *n))
            .filter(|(s, _)| *s > 0.85)
            .max_by(|a, b| a.0.total_cmp(&b.0))
        {
            message.push_str(&format!(": did you mean `{}`?", near.1));
        } else if !names.is_empty() {
            message.push_str(&format!(": expected one of {}", names.join(", ")));
        }
        out.push(Diagnostic {
            range: range(text, k.start, k.end, enc),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("heph".to_string()),
            message,
            ..Default::default()
        });
        flagged.push((k.start, k.name));
    }
    // An empty file is the default config, as the loader reads it.
    if text.is_empty() {
        return out;
    }
    if let Err(e) = serde_yaml::from_str::<hconfig::ConfigYaml>(text) {
        let at = parser_offset(text, &e);
        let msg = e.to_string();
        let named = unknown_field(&msg);
        // The unknown-key check already said it, with a suggestion: the error
        // sits on that key, or names it (a hand-rolled type's error sits on
        // its mapping instead).
        let said = flagged
            .iter()
            .any(|(start, name)| *start == at || named == Some(name.as_str()));
        if !said {
            let token = text.get(at..).map_or(0, |r| {
                r.find(|c: char| c.is_whitespace()).unwrap_or(r.len())
            });
            out.push(Diagnostic {
                range: range(text, at, at + token, enc),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("heph".to_string()),
                message: parser_message(&msg),
                ..Default::default()
            });
        }
    }
    out.sort_by_key(|d| (d.range.start.line, d.range.start.character));
    out
}

/// The byte offset of the parser's error, from its line and column (a column
/// counts characters, after any byte-order mark).
fn parser_offset(text: &str, e: &serde_yaml::Error) -> usize {
    let Some(loc) = e.location() else {
        return 0;
    };
    lines(text)
        .nth(loc.line().saturating_sub(1))
        .map_or(text.len(), |(start, line)| {
            start
                + line
                    .char_indices()
                    .nth(loc.column().saturating_sub(1))
                    .map_or(line.len(), |(i, _)| i)
        })
}

/// The key a serde unknown-field error names: `` unknown field `X` ``, or the
/// plugin entry's `` unknown plugin field `X` ``.
fn unknown_field(msg: &str) -> Option<&str> {
    ["unknown field `", "unknown plugin field `"]
        .iter()
        .find_map(|p| msg.split_once(p)?.1.split_once('`').map(|(name, _)| name))
}

/// The parser's message without its ` at line N column M` suffix: the
/// diagnostic's range already says where.
fn parser_message(msg: &str) -> String {
    match msg.rfind(" at line ") {
        Some(i)
            if msg.get(i + " at line ".len()..).is_some_and(|t| {
                t.bytes()
                    .all(|b| b.is_ascii_digit() || b" column".contains(&b))
            }) =>
        {
            msg.get(..i).unwrap_or(msg).to_string()
        }
        _ => msg.to_string(),
    }
}

#[cfg(test)]
mod tests;
