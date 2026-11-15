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
//! Flow collections (`{a: 1}`, `[a]`) and multi-line plain scalars are values
//! it does not look inside; block scalars (`|`, `>`) are skipped.

use std::sync::LazyLock;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Diagnostic, DiagnosticSeverity,
    Documentation, Hover, HoverContents, MarkupContent, MarkupKind, Range, TextEdit,
};
use serde_json::Value;

use super::position::{self, Encoding};

/// Built once per server: the schema never changes under a running binary.
static SCHEMA: LazyLock<Value> = LazyLock::new(hconfig::config_json_schema);

/// One step of a key path: a mapping key, or a sequence item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    Key(String),
    Item,
}

/// An open mapping key or sequence item, while scanning.
#[derive(Clone, Debug)]
struct Frame {
    /// The column of the key, or of the `-`.
    indent: usize,
    seg: Seg,
    /// A key with nothing after its `:`, which a `-` at its own column
    /// continues (`plugins:` then `- builtin: x`).
    open: bool,
    /// Tells two items of one sequence apart, so their keys are not siblings.
    id: usize,
}

/// One `key:` of the document, with where it sits.
#[derive(Clone, Debug)]
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
    let mut i = bytes.iter().take_while(|b| **b == b' ').count();
    let indent = i;
    let mut dashes = Vec::new();
    while bytes.get(i) == Some(&b'-') && matches!(bytes.get(i + 1), None | Some(b' ' | b'\t')) {
        dashes.push(i);
        i += 1;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
    }
    let content = i;
    let key = key_at(line, content);
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
        dashes,
        content,
        key,
        value,
    }
}

/// The `key:` starting at column `at`, if one does: a plain key ends at the
/// first `:` followed by a blank or the line's end; a quoted one at its
/// closing quote, which a `:` must follow.
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
    if matches!(first, b'#' | b'{' | b'[' | b'&' | b'*' | b'!' | b'|' | b'>') {
        return None;
    }
    let mut j = at;
    while let Some(&b) = bytes.get(j) {
        if colon_ok(j) {
            let name = line.get(at..j)?.trim_end();
            return Some(KeyTok {
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

/// `text` up to a ` #` comment that is not inside quotes.
fn strip_comment(text: &str) -> &str {
    let mut quote: Option<u8> = None;
    let mut prev_blank = true;
    for (i, b) in text.bytes().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'#' && prev_blank => return text.get(..i).unwrap_or(text),
            None => {}
        }
        prev_blank = b == b' ' || b == b'\t';
    }
    text
}

/// The block structure above a position: the stack of open keys and items.
#[derive(Default)]
struct Scan {
    stack: Vec<Frame>,
    /// Inside a block scalar opened by a key at this column: lines deeper than
    /// it are text.
    block_scalar: Option<usize>,
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

    fn path(&self) -> Vec<Seg> {
        self.stack.iter().map(|f| f.seg.clone()).collect()
    }

    fn ids(&self) -> Vec<usize> {
        self.stack.iter().map(|f| f.id).collect()
    }

    /// Whether a line at this indent is block-scalar text. Closes the scalar
    /// at the first line that is not.
    fn in_block_scalar(&mut self, s: &Shape<'_>, blank: bool) -> bool {
        match self.block_scalar {
            Some(k) if blank || s.indent > k => true,
            Some(_) => {
                self.block_scalar = None;
                false
            }
            None => false,
        }
    }

    /// Feed one whole line; the key it declares, if any.
    fn feed(&mut self, line_start: usize, line: &str) -> Option<KeyEntry> {
        let s = shape(line);
        let blank =
            s.dashes.is_empty() && matches!(line.as_bytes().get(s.content), None | Some(b'#'));
        if self.in_block_scalar(&s, blank) || blank {
            return None;
        }
        for &d in &s.dashes {
            self.pop_for_dash(d);
            self.push(d, Seg::Item, true);
        }
        let key = s.key?;
        self.pop_for_key(key.start);
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

/// Each line with its absolute start, without its line break.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    text.split_inclusive('\n').map(move |l| {
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
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let rest = text.get(start..)?;
    let end = rest.find('\n').unwrap_or(rest.len());
    Some((start, rest.get(..end)?.trim_end_matches('\r')))
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
/// comment, or after a scalar list item: no key or value of the schema's.
pub(crate) fn cursor(text: &str, offset: usize) -> Option<Cursor> {
    let (line_start, line) = line_at(text, offset)?;
    let col = offset - line_start;
    let mut scan = Scan::default();
    for (start, l) in lines(text.get(..line_start)?) {
        scan.feed(start, l);
    }
    let full = shape(line);
    let blank = matches!(line.as_bytes().get(full.content), None | Some(b'#'));
    if scan.in_block_scalar(&full, blank && full.dashes.is_empty()) {
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
/// `null` left out.
fn branches(v: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    let mut todo = vec![v];
    while let Some(v) = todo.pop() {
        let v = deref(v);
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
        if b.get("type").and_then(Value::as_str) == Some("boolean") {
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

/// A doc comment as Markdown for an editor: rustdoc intra-doc links
/// (`` [`X`] ``, `` [`x`](Self::x) ``) become their text, URLs stay links.
fn clean_doc(doc: &str) -> String {
    let mut out = String::with_capacity(doc.len());
    let mut rest = doc;
    while let Some(open) = rest.find('[') {
        out.push_str(rest.get(..open).unwrap_or_default());
        let after = rest.get(open + 1..).unwrap_or_default();
        let Some(close) = after.find(']') else {
            out.push_str(rest.get(open..).unwrap_or_default());
            return out;
        };
        let inner = after.get(..close).unwrap_or_default();
        let tail = after.get(close + 1..).unwrap_or_default();
        if let Some(target) = tail.strip_prefix('(')
            && let Some(end) = target.find(')')
        {
            let url = target.get(..end).unwrap_or_default();
            if url.starts_with("http") {
                out.push_str(&format!("[{inner}]({url})"));
            } else {
                out.push_str(inner);
            }
            rest = target.get(end + 1..).unwrap_or_default();
        } else {
            out.push_str(inner);
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn display_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Item => out.push_str("[]"),
        }
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
            properties(parent)
                .into_iter()
                .filter(|(k, _)| k.starts_with(prefix.as_str()) && !present.iter().any(|p| p == k))
                .map(|(k, schema)| {
                    let ty = type_summary(schema);
                    let nested = ["object", "list", "map"].contains(&ty.as_str());
                    let new_text = match (has_colon, nested) {
                        (true, _) => k.to_string(),
                        (false, true) => format!("{k}:"),
                        (false, false) => format!("{k}: "),
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
    let mut scan = Scan::default();
    for (start, l) in lines(text.get(..line_start)?) {
        scan.feed(start, l);
    }
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

/// The parser's error, if the document does not load, then every key the
/// schema does not know.
pub(crate) fn diagnostics(text: &str, enc: Encoding) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
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
    }
    // An empty file is the default config, as the loader reads it.
    if text.is_empty() {
        return out;
    }
    if let Err(e) = serde_yaml::from_str::<hconfig::ConfigYaml>(text) {
        let at = e.location().map_or(0, |l| l.index()).min(text.len());
        let pos = position::to_position(text, at, enc);
        // The unknown-key check already said it, with a suggestion.
        if out.iter().any(|d| d.range.start.line == pos.line) {
            return out;
        }
        let token = text.get(at..).map_or(0, |r| {
            r.find(|c: char| c.is_whitespace()).unwrap_or(r.len())
        });
        out.push(Diagnostic {
            range: range(text, at, at + token, enc),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("heph".to_string()),
            message: parser_message(&e),
            ..Default::default()
        });
    }
    out.sort_by_key(|d| (d.range.start.line, d.range.start.character));
    out
}

/// The parser's message without its ` at line N column M` suffix: the
/// diagnostic's range already says where.
fn parser_message(e: &serde_yaml::Error) -> String {
    let msg = e.to_string();
    match msg.rfind(" at line ") {
        Some(i)
            if msg.get(i + " at line ".len()..).is_some_and(|t| {
                t.bytes()
                    .all(|b| b.is_ascii_digit() || b" column".contains(&b))
            }) =>
        {
            msg.get(..i).unwrap_or(&msg).to_string()
        }
        _ => msg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENC: Encoding = Encoding::Utf8;

    /// `text` with `|` marking the cursor: the text without it, and the offset.
    fn at(marked: &str) -> (String, usize) {
        let offset = marked.find('|').expect("cursor mark");
        (marked.replacen('|', "", 1), offset)
    }

    fn labels(marked: &str) -> Vec<String> {
        let (text, offset) = at(marked);
        completion(&text, offset, ENC)
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    fn key(k: &str) -> Seg {
        Seg::Key(k.to_string())
    }

    fn cursor_path(marked: &str) -> Option<(Vec<Seg>, String, bool)> {
        let (text, offset) = at(marked);
        match cursor(&text, offset)? {
            Cursor::Key {
                parent_path,
                prefix,
                ..
            } => Some((parent_path, prefix, true)),
            Cursor::Value { path, prefix, .. } => Some((path, prefix, false)),
        }
    }

    /// Where a cursor lands: the path, the prefix, and whether it is a key.
    type Landing<'a> = Option<(Vec<Seg>, &'a str, bool)>;

    /// The adversarial table: each row is a cursor, and where it must land.
    #[test]
    fn cursor_classification() {
        let rows: Vec<(&str, Landing<'_>)> = vec![
            ("|", Some((vec![], "", true))),
            ("ca|", Some((vec![], "ca", true))),
            ("cache:\n  sp|", Some((vec![key("cache")], "sp", true))),
            ("cache:\n|", Some((vec![], "", true))),
            ("cache:\n  |", Some((vec![key("cache")], "", true))),
            // A sibling at the parent's column closes it.
            ("cache:\n  a: 1\nlo|", Some((vec![], "lo", true))),
            (
                "lock:\n  backend: f|",
                Some((vec![key("lock"), key("backend")], "f", false)),
            ),
            (
                "lock:\n  backend: |",
                Some((vec![key("lock"), key("backend")], "", false)),
            ),
            (
                "lock:\n  backend: \"m|\"",
                Some((vec![key("lock"), key("backend")], "m", false)),
            ),
            // A sequence at the key's own column, and indented.
            (
                "plugins:\n- bu|",
                Some((vec![key("plugins"), Seg::Item], "bu", true)),
            ),
            (
                "plugins:\n  - bu|",
                Some((vec![key("plugins"), Seg::Item], "bu", true)),
            ),
            (
                "plugins:\n  - builtin: x\n    op|",
                Some((vec![key("plugins"), Seg::Item], "op", true)),
            ),
            // The second item is its own mapping.
            (
                "plugins:\n  - builtin: x\n  - |",
                Some((vec![key("plugins"), Seg::Item], "", true)),
            ),
            // Comments and blank lines change nothing.
            (
                "cache:\n  # note\n\n  sp|",
                Some((vec![key("cache")], "sp", true)),
            ),
            // A block scalar's text is not structure (`>` here: `|` is the
            // cursor mark).
            ("version: >\n  ca|", None),
            ("version: >-\n  text\n\n  ca|", None),
            ("version: >\n  text\nca|", Some((vec![], "ca", true))),
            // Flow collections and comments are not looked inside.
            ("fuse: {ena|", None),
            ("# ca|", None),
            // A quoted key.
            ("\"cache\":\n  sp|", Some((vec![key("cache")], "sp", true))),
            // CRLF line ends.
            ("cache:\r\n  sp|", Some((vec![key("cache")], "sp", true))),
            // A value containing `:` is not a second key, nor a value the
            // schema could complete.
            ("caches:\n  main:\n    uri: s3://b|", None),
            (
                "caches:\n  main:\n    uri: s3|",
                Some((vec![key("caches"), key("main"), key("uri")], "s3", false)),
            ),
            // `key:` with no blank after it is not yet a value position.
            ("lock:\n  backend:|", None),
            // Past a scalar list item.
            ("fs:\n  skip:\n    - a b|", None),
        ];
        for (marked, want) in rows {
            let got = cursor_path(marked);
            let want = want.map(|(p, s, k)| (p, s.to_string(), k));
            assert_eq!(got, want, "{marked:?}");
        }
    }

    #[test]
    fn completes_top_level_keys_minus_present() {
        let got = labels("version: v1\n|");
        assert!(got.contains(&"cache".to_string()), "{got:?}");
        assert!(got.contains(&"plugins".to_string()), "{got:?}");
        assert!(!got.contains(&"version".to_string()), "{got:?}");
    }

    #[test]
    fn completes_nested_keys_and_inserts_a_colon() {
        let (text, offset) = at("cache:\n  sp|");
        let items = completion(&text, offset, ENC);
        assert_eq!(
            items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["spillThresholdBytes"]
        );
        let Some(CompletionTextEdit::Edit(edit)) = &items[0].text_edit else {
            panic!("no edit")
        };
        assert_eq!(edit.new_text, "spillThresholdBytes: ");
    }

    #[test]
    fn item_keys_are_per_item() {
        let got = labels("plugins:\n  - builtin: a\n  - |");
        assert!(got.contains(&"builtin".to_string()), "{got:?}");
        let got = labels("plugins:\n  - builtin: a\n    |");
        assert!(!got.contains(&"builtin".to_string()), "{got:?}");
        assert!(got.contains(&"options".to_string()), "{got:?}");
    }

    #[test]
    fn completes_enum_and_boolean_values() {
        assert_eq!(labels("lock:\n  backend: |"), ["fs", "mem"]);
        assert_eq!(labels("lock:\n  backend: m|"), ["mem"]);
        assert_eq!(labels("telemetry:\n  enabled: |"), ["true", "false"]);
        let fuse = labels("fuse:\n  enabled: |");
        assert!(fuse.contains(&"auto".to_string()), "{fuse:?}");
        assert!(fuse.contains(&"true".to_string()), "{fuse:?}");
        assert_eq!(
            labels("resources:\n  gpu:\n    unit: |"),
            ["count", "cpu", "bytes"]
        );
    }

    #[test]
    fn map_values_complete_their_entry_schema() {
        let got = labels("caches:\n  main:\n    |");
        assert!(got.contains(&"uri".to_string()), "{got:?}");
        assert!(got.contains(&"region".to_string()), "{got:?}");
        let got = labels("resources:\n  |");
        assert!(got.contains(&"cpu".to_string()), "{got:?}");
        assert!(got.contains(&"memory".to_string()), "{got:?}");
    }

    #[test]
    fn hover_shows_the_field_doc() {
        let (text, offset) = at("cache:\n  spill|ThresholdBytes: 10\n");
        let h = hover(&text, offset, ENC).expect("hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(
            m.value.starts_with("`cache.spillThresholdBytes`: integer"),
            "{}",
            m.value
        );
        assert!(m.value.contains("spill to plain files"), "{}", m.value);
        let (text, offset) = at("lock:\n  back|end: fs\n");
        let HoverContents::Markup(m) = hover(&text, offset, ENC).expect("hover").contents else {
            panic!("markup")
        };
        assert!(m.value.contains("One of `fs`, `mem`."), "{}", m.value);
    }

    #[test]
    fn hover_cleans_intra_doc_links() {
        assert_eq!(clean_doc("See [`ScratchConfig`]."), "See `ScratchConfig`.");
        assert_eq!(clean_doc("a [`x`](Self::x) b"), "a `x` b");
        assert_eq!(
            clean_doc("[docs](https://example.com)"),
            "[docs](https://example.com)"
        );
        assert_eq!(clean_doc("unclosed [a"), "unclosed [a");
    }

    fn messages(text: &str) -> Vec<String> {
        diagnostics(text, ENC)
            .into_iter()
            .map(|d| {
                format!(
                    "{}:{} {}",
                    d.range.start.line, d.range.start.character, d.message
                )
            })
            .collect()
    }

    #[test]
    fn valid_configs_have_no_diagnostics() {
        for text in [
            "",
            "version: v1.2.3\n",
            "plugins:\n- builtin: exec\n  options:\n    anything: 1\n",
            "caches:\n  main:\n    uri: s3://b/p\n    read: true\n",
            "resources:\n  cpu: 50%\n  gpu:\n    unit: count\n    value: 2\n",
            "scratch:\n  restoreScopes:\n    - master\n",
            // Block-scalar text that looks like a key is text.
            "version: |\n  notakey: x\n",
            "# only a comment\nversion: v1 # trailing\n",
        ] {
            assert_eq!(messages(text), Vec::<String>::new(), "{text:?}");
        }
    }

    #[test]
    fn every_unknown_key_is_flagged_with_a_suggestion() {
        let got = messages("cahce:\n  x: 1\nlock:\n  backnd: fs\n");
        assert_eq!(
            got,
            [
                "0:0 unknown key `cahce` at the top level: did you mean `cache`?",
                "3:2 unknown key `backnd` in `lock`: did you mean `backend`?",
            ]
        );
    }

    #[test]
    fn parser_errors_land_on_their_location() {
        let got = messages("lock:\n  backend: disk\n");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].starts_with("1:"), "{got:?}");
        assert!(got[0].contains("disk"), "{got:?}");
        assert!(!got[0].contains(" at line "), "{got:?}");
        let got = messages("resources:\n  Bad: 1\n");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("not a valid pool name"), "{got:?}");
        let got = messages("plugins:\n- builtin: a\n  path: b\n");
        assert!(got.iter().any(|m| m.contains("exactly one")), "{got:?}");
    }

    #[test]
    fn syntax_errors_are_reported() {
        let got = messages("cache: [\n");
        assert_eq!(got.len(), 1, "{got:?}");
    }
}
