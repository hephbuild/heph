//! A transparent middleware that sits between the editor (stdio) and the
//! `starlark_lsp` server (an in-memory connection). It forwards every message
//! untouched except `textDocument/hover` and `textDocument/completion`: those are
//! forwarded too, but their responses are enriched on the way back with heph
//! specifics the stock server can't know about —
//! - hover gains a "Targets" block (the addresses produced by the
//!   symbol under the cursor), and
//! - completion gains the config fields of the target's `driver` (from the
//!   driver's [`schema`](heph_plugin::driver::Driver::schema)).

use super::index::SharedState;
use crate::pluginbuildfile::run_file::resolve_load_target;
use lsp_server::{Connection, Message, Notification, RequestId};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Hover, HoverContents, LocationLink,
    MarkedString, MarkupContent, MarkupKind, Position, Range,
};
use starlark_lsp::server::LspUri;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tower_lsp_server::UriExt as _;

/// What an intercepted request was, so its response can be enriched.
#[derive(Clone)]
enum Pending {
    Hover {
        uri: LspUri,
        line: u32,
        col: u32,
    },
    Completion {
        uri: LspUri,
        line: u32,
        col: u32,
    },
    Definition {
        uri: LspUri,
        line: u32,
        col: u32,
    },
    /// The `initialize` handshake: its response's capabilities are patched so
    /// the editor fires completion after `.` (member completion).
    Initialize,
}

/// Pump messages between `client` (editor stdio) and `server` (starlark_lsp),
/// enriching hover/completion responses. Returns when either side closes.
pub(crate) fn run(client: &Connection, server: &Connection, shared: Arc<SharedState>) {
    let pending: Arc<Mutex<HashMap<RequestId, Pending>>> = Arc::new(Mutex::new(HashMap::new()));

    std::thread::scope(|scope| {
        // client → server: forward, recording hover/completion requests.
        scope.spawn(|| {
            for msg in &client.receiver {
                if let Message::Request(req) = &msg
                    && let Some(p) = classify(&req.method, &req.params)
                {
                    pending.lock().expect("pending").insert(req.id.clone(), p);
                }
                let stop = matches!(&msg, Message::Notification(n) if n.method == "exit");
                if server.sender.send(msg).is_err() || stop {
                    break;
                }
            }
            // The client side ended — either an `exit` notification, or stdin
            // closed (the editor disconnected without the shutdown/exit handshake).
            // Tell the inner server to exit so its main loop returns and the
            // server→client thread unblocks; otherwise the process would hang.
            // Ignore the result: the inner server may already be gone.
            let _sent = server.sender.send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: serde_json::Value::Null,
            }));
        });

        // server → client: forward, enriching recorded responses.
        scope.spawn(|| {
            for msg in &server.receiver {
                let out = match msg {
                    Message::Response(mut resp) => {
                        if let Some(p) = pending.lock().expect("pending").remove(&resp.id) {
                            enrich(&mut resp, &p, &shared);
                        }
                        Message::Response(resp)
                    }
                    other => other,
                };
                if client.sender.send(out).is_err() {
                    break;
                }
            }
        });
    });
}

/// Extract `(uri, line, col)` for the requests we enrich. Positions are 0-based,
/// as sent by the client.
fn classify(method: &str, params: &serde_json::Value) -> Option<Pending> {
    // `initialize` carries no position; handle it before the position extraction.
    if method == "initialize" {
        return Some(Pending::Initialize);
    }
    let pos = params.get("position")?;
    let line = pos.get("line")?.as_u64()? as u32;
    let col = pos.get("character")?.as_u64()? as u32;
    let uri_str = params.get("textDocument")?.get("uri")?.as_str()?;
    let uri = LspUri::try_from(uri_str.parse::<lsp_types::Uri>().ok()?).ok()?;
    match method {
        "textDocument/hover" => Some(Pending::Hover { uri, line, col }),
        "textDocument/completion" => Some(Pending::Completion { uri, line, col }),
        "textDocument/definition" => Some(Pending::Definition { uri, line, col }),
        _ => None,
    }
}

fn enrich(resp: &mut lsp_server::Response, pending: &Pending, shared: &SharedState) {
    match pending {
        Pending::Hover { uri, line, col } => enrich_hover(resp, uri, *line, *col, shared),
        Pending::Completion { uri, line, col } => enrich_completion(resp, uri, *line, *col, shared),
        Pending::Definition { uri, line, col } => enrich_definition(resp, uri, *line, *col, shared),
        Pending::Initialize => enrich_initialize(resp),
    }
}

/// Advertise `.` as a completion trigger character so editors request completion
/// right after a member access (`heph.`, `heph.fs.`). The stock server sends an
/// empty `completionProvider`, so nothing would fire on `.` otherwise.
fn enrich_initialize(resp: &mut lsp_server::Response) {
    let Some(serde_json::Value::Object(result)) = resp.result.as_mut() else {
        return;
    };
    let caps = result
        .entry("capabilities")
        .or_insert_with(|| serde_json::json!({}));
    let serde_json::Value::Object(caps) = caps else {
        return;
    };
    let cp = caps
        .entry("completionProvider")
        .or_insert_with(|| serde_json::json!({}));
    if let serde_json::Value::Object(cp) = cp {
        cp.insert("triggerCharacters".to_string(), serde_json::json!(["."]));
    }
}

/// Resolve goto-definition for a `load`-imported symbol across every BUILD file
/// in its package. The stock server only parses one file per package, so a symbol
/// defined in a sibling file resolves to the `load(...)` line (or nothing); we
/// scan the whole package and point at the real definition.
fn enrich_definition(
    resp: &mut lsp_server::Response,
    uri: &LspUri,
    line: u32,
    col: u32,
    shared: &SharedState,
) {
    let LspUri::File(doc_path) = uri else {
        return;
    };
    let Some(index) = shared.index(uri) else {
        return;
    };
    let Some((load_path, exported)) = index.loaded_symbol_at(line + 1, col + 1) else {
        return;
    };

    // If the stock server already resolved into another file, it's correct — leave it.
    if result_points_to_other_file(resp.result.as_ref(), doc_path) {
        return;
    }

    let current_pkg = pkg_of(&shared.root, doc_path);
    let Ok(resolved) = resolve_load_target(&shared.root, &current_pkg, load_path) else {
        return;
    };
    // The load target is a package dir (scan every BUILD file) or an explicit file.
    let files: Vec<std::path::PathBuf> = if resolved.is_dir() {
        package_build_files(&resolved, &shared.patterns)
    } else {
        vec![resolved]
    };

    for file in files {
        if let Some((dl, c0, c1)) = find_symbol_def(&file, exported) {
            let Some(uri) = lsp_types::Uri::from_file_path(&file) else {
                continue;
            };
            let target_range = Range {
                start: Position::new(dl, c0),
                end: Position::new(dl, c1),
            };
            let link = LocationLink {
                origin_selection_range: None,
                target_uri: uri,
                target_range,
                target_selection_range: target_range,
            };
            resp.result = Some(serde_json::to_value(vec![link]).expect("serialize definition"));
            resp.error = None;
            return;
        }
    }
}

/// Whether a goto response already points at a file other than `doc_path` (i.e.
/// the stock server resolved it cross-file and we should not override).
fn result_points_to_other_file(result: Option<&serde_json::Value>, doc_path: &Path) -> bool {
    let Some(value) = result else { return false };
    let doc = lsp_types::Uri::from_file_path(doc_path);
    let targets = match value {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(_) => vec![value.clone()],
        _ => return false,
    };
    targets.iter().any(|t| {
        let uri = t
            .get("targetUri")
            .or_else(|| t.get("uri"))
            .and_then(|u| u.as_str());
        match (uri, &doc) {
            (Some(u), Some(d)) => u != d.as_str(),
            (Some(_), None) => true,
            _ => false,
        }
    })
}

/// Files in `dir` matching a BUILD pattern, sorted by name (heph's merge order).
fn package_build_files(dir: &Path, patterns: &[glob::Pattern]) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            patterns
                .iter()
                .any(|p| p.matches(&e.file_name().to_string_lossy()))
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Find the top-level definition of `name` (a `def` or an assignment) in `path`,
/// returning the 0-based `(line, name_col_start, name_col_end)`. Identifiers are
/// ASCII, so byte offsets equal columns.
fn find_symbol_def(path: &Path, name: &str) -> Option<(u32, u32, u32)> {
    let content = std::fs::read_to_string(path).ok()?;
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        let is_def = trimmed
            .strip_prefix("def ")
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix(name))
            .is_some_and(|r| r.trim_start().starts_with('('));
        let is_assign = trimmed
            .strip_prefix(name)
            .map(str::trim_start)
            .is_some_and(|r| r.starts_with('=') && !r.starts_with("=="));
        if is_def || is_assign {
            let col = line.find(name)? as u32;
            return Some((i as u32, col, col + name.len() as u32));
        }
    }
    None
}

/// Package name for a BUILD-file path: its parent dir, workspace-relative,
/// forward-slashed (empty at the root).
fn pkg_of(root: &Path, path: &Path) -> String {
    let parent = path.parent().unwrap_or(path);
    parent
        .strip_prefix(root)
        .unwrap_or(parent)
        .to_string_lossy()
        .replace('\\', "/")
}

fn enrich_hover(
    resp: &mut lsp_server::Response,
    uri: &LspUri,
    line: u32,
    col: u32,
    shared: &SharedState,
) {
    let Some(index) = shared.index(uri) else {
        return;
    };

    // Index positions are 1-based.
    let mut md = existing_hover_markdown(resp);

    // Hovering a provider function reference (`heph.<provider>.<fn>`, e.g.
    // `heph.fs.join`) → its rendered signature + doc, pulled from the function
    // registry. Authoritative for these, so it replaces any stock hover (the
    // stock server has no docs for the engine-injected native functions).
    if let Some(fn_md) = provider_fn_hover(&index.source, line, col, shared) {
        md = fn_md;
    } else if md.is_empty()
        && let Some(doc) = index.def_hover_at(line + 1, col + 1)
    {
        // Otherwise, if the stock server produced no hover (e.g. an undocumented
        // `def`), fall back to the local function's rendered signature.
        md.push_str(doc);
    }

    // Append the targets this call site produced, if any.
    let targets = index.targets_at(line + 1, col + 1).unwrap_or(&[]);
    if md.is_empty() && targets.is_empty() {
        return;
    }
    if !targets.is_empty() {
        if !md.is_empty() {
            md.push_str("\n\n---\n\n");
        }
        md.push_str(&format!("**Targets ({})**\n\n```\n", targets.len()));
        for a in targets {
            md.push_str(a);
            md.push('\n');
        }
        md.push_str("```\n");
    }

    let hover = Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: None,
    };
    resp.result = Some(serde_json::to_value(hover).expect("serialize hover"));
    resp.error = None;
}

/// Hover markdown for a provider-function reference under the cursor, or `None`
/// if the cursor is not on a `heph.<provider>.<fn>` whose function is registered.
///
/// This can't be left to the stock `starlark_lsp`: its hover resolves top-level
/// globals and `load()`-ed symbols, but not a member of a *global namespace*
/// (`heph` → `fs` → `join`) — hovering one resolves only as far as the top-level
/// `heph` value (even for the `heph.core` builtins). The native registration
/// already supplies these functions' signature/param types (see
/// `run_file::build_globals`) yet the stock server still produces no member
/// hover, so we render it here from the function registry.
///
/// To look and color identically to a local `def`, build a starlark
/// [`DocFunction`] from the declared signature and run it through the same
/// `render_doc_item_no_link` the stock server uses — yielding a
/// `def name(p: str) -> str` prototype with Starlark type names.
fn provider_fn_hover(source: &str, line: u32, col: u32, shared: &SharedState) -> Option<String> {
    use heph_core::htvalue::signature::Param;
    use starlark::docs::markdown::render_doc_item_no_link;
    use starlark::docs::{
        DocFunction, DocItem, DocMember, DocParam, DocParams, DocReturn, DocString, DocStringKind,
    };

    let line_text = source.lines().nth(line as usize)?;
    let (provider, func) = provider_fn_at(line_text, col as usize)?;
    // `heph.core.<fn>` builtins aren't in the registry — their doc is pre-rendered.
    if provider == "core" {
        return shared
            .core_members
            .iter()
            .find(|m| m.name == func)
            .map(|m| m.doc.clone());
    }
    let registry = shared.engine.provider_function_registry();
    let rf = registry.get(&provider, &func)?;
    let sig = &rf.signature;

    let ty = |p: &Param| crate::pluginbuildfile::run_file::param_type_to_ty(&p.ty);
    let to_param = |p: &Param| DocParam {
        name: p.name.to_string(),
        docs: None,
        typ: ty(p),
        default_value: p.default.as_ref().map(default_repr),
    };
    let params = DocParams {
        pos_only: Vec::new(),
        pos_or_named: sig.positional.iter().map(to_param).collect(),
        // A `*args`-style variadic: each element typed as the variadic's type.
        args: sig.variadic.as_ref().map(|p| DocParam {
            name: p.name.to_string(),
            docs: None,
            typ: ty(p),
            default_value: None,
        }),
        named_only: sig.named.iter().map(to_param).collect(),
        kwargs: None,
    };
    let item = DocItem::Member(DocMember::Function(DocFunction {
        docs: DocString::from_docstring(DocStringKind::Starlark, &rf.doc),
        params,
        ret: DocReturn {
            docs: None,
            typ: crate::pluginbuildfile::run_file::param_type_to_ty(&sig.returns),
        },
    }));
    // Render under the bare function name so the prototype reads `def join(...)`
    // — identical to a local `def` — rather than `def heph.fs.join(...)`. The
    // namespace is evident from the call site being hovered.
    Some(render_doc_item_no_link(&func, &item))
}

/// Render an htvalue default as a Starlark literal for the hover prototype
/// (`name: ty = <repr>`). Best-effort: containers collapse to `[]`/`[...]` etc.
fn default_repr(v: &heph_core::htvalue::Value) -> String {
    use heph_core::htvalue::Value;
    match v {
        Value::Null() => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Uint(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => format!("{s:?}"),
        Value::List(xs) if xs.is_empty() => "[]".to_string(),
        Value::List(_) => "[...]".to_string(),
        Value::Map(m) if m.is_empty() => "{}".to_string(),
        Value::Map(_) => "{...}".to_string(),
    }
}

/// If the identifier at byte offset `col` on `line` is the final segment of a
/// `heph.<provider>.<fn>` dotted path, return `(provider, fn)`. The cursor may
/// sit anywhere within the function identifier. Only ASCII identifiers (the
/// namespace/provider/function names) and `.` separators are walked.
fn provider_fn_at(line: &str, col: usize) -> Option<(String, String)> {
    let b = line.as_bytes();
    let is_ident = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    let col = col.min(b.len());

    // Identifier containing the cursor (cursor may be at either edge of the word).
    let mut end = col;
    while b.get(end).is_some_and(is_ident) {
        end += 1;
    }
    let mut start = col;
    while start > 0 && b.get(start - 1).is_some_and(is_ident) {
        start -= 1;
    }
    if start == end {
        return None; // cursor is not on an identifier
    }

    // Walk left across `.ident` segments preceding the cursor's identifier.
    let mut segments = vec![line.get(start..end)?];
    let mut i = start;
    while i > 0 && b.get(i - 1) == Some(&b'.') {
        let dot = i - 1;
        let mut s = dot;
        while s > 0 && b.get(s - 1).is_some_and(is_ident) {
            s -= 1;
        }
        if s == dot {
            break; // a `.` with no identifier before it
        }
        segments.push(line.get(s..dot)?);
        i = s;
    }
    segments.reverse();

    // Exactly `heph.<provider>.<fn>`; the `heph.core.*` builtins live in a
    // different namespace and aren't in the provider registry.
    match segments.as_slice() {
        ["heph", provider, func] => Some((provider.to_string(), func.to_string())),
        _ => None,
    }
}

/// Pull whatever markdown/plain text the stock server already produced for hover.
fn existing_hover_markdown(resp: &lsp_server::Response) -> String {
    let Some(value) = &resp.result else {
        return String::new();
    };
    let Ok(hover) = serde_json::from_value::<Hover>(value.clone()) else {
        return String::new();
    };
    match hover.contents {
        HoverContents::Markup(m) => m.value,
        HoverContents::Scalar(s) => marked_string_text(s),
        HoverContents::Array(items) => items
            .into_iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn marked_string_text(s: MarkedString) -> String {
    match s {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}

/// Completion items for a `heph` namespace member access whose dotted base ends
/// just before the cursor. `heph.` → the provider namespaces (`fs`, `go`, …) plus
/// `core`; `heph.<provider>.` → that provider's functions, with their signature
/// as detail and doc as the popup. Empty when the cursor isn't on such a member.
fn provider_member_completions(prefix: &str, shared: &SharedState) -> Vec<CompletionItem> {
    let Some(base) = completion_member_base(prefix) else {
        return vec![];
    };
    let registry = shared.engine.provider_function_registry();
    match base.as_slice() {
        // `heph.` → namespace names. `core` is a static builtin namespace, the
        // rest come from the providers that registered functions.
        ["heph"] => {
            let mut names: Vec<String> = registry.providers().map(|(p, _)| p.to_string()).collect();
            names.push("core".to_string());
            names.sort();
            names.dedup();
            names
                .into_iter()
                .map(|name| CompletionItem {
                    kind: Some(CompletionItemKind::MODULE),
                    detail: Some("heph namespace".to_string()),
                    sort_text: Some(format!("0_{name}")),
                    label: name,
                    ..Default::default()
                })
                .collect()
        }
        // `heph.core.` → the static `heph.core` builtins (not in the registry).
        ["heph", "core"] => shared
            .core_members
            .iter()
            .map(|m| CompletionItem {
                kind: Some(CompletionItemKind::FUNCTION),
                detail: (!m.detail.is_empty()).then(|| m.detail.clone()),
                documentation: (!m.doc.is_empty()).then(|| {
                    lsp_types::Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: m.doc.clone(),
                    })
                }),
                sort_text: Some(format!("0_{}", m.name)),
                label: m.name.clone(),
                ..Default::default()
            })
            .collect(),
        // `heph.<provider>.` → that provider's functions.
        ["heph", provider] => registry
            .providers()
            .find(|(p, _)| p == provider)
            .map(|(_, fns)| {
                let mut items: Vec<CompletionItem> = fns
                    .iter()
                    .map(|(name, rf)| CompletionItem {
                        kind: Some(CompletionItemKind::FUNCTION),
                        detail: Some(rf.signature.render(name)),
                        documentation: (!rf.doc.is_empty())
                            .then(|| lsp_types::Documentation::String(rf.doc.clone())),
                        sort_text: Some(format!("0_{name}")),
                        label: name.clone(),
                        ..Default::default()
                    })
                    .collect();
                items.sort_by(|a, b| a.label.cmp(&b.label));
                items
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

/// The dotted base of a member access ending just before the cursor: the segments
/// before the final `.<partial>`. For `… = heph.fs.gl` returns `[heph, fs]`;
/// for `heph.` returns `[heph]`. `None` when the text before the cursor isn't a
/// `<ident>(.<ident>)*.` member access. Walks ASCII identifiers + `.` only.
fn completion_member_base(prefix: &str) -> Option<Vec<&str>> {
    let b = prefix.as_bytes();
    let is_ident = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    let mut i = b.len();
    // Skip the (possibly empty) partial member the user is typing.
    while i > 0 && b.get(i - 1).is_some_and(is_ident) {
        i -= 1;
    }
    // It must be a member access: an identifier preceded by a `.`.
    if i == 0 || b.get(i - 1) != Some(&b'.') {
        return None;
    }
    i -= 1; // consume the `.`
    let mut segments: Vec<&str> = Vec::new();
    loop {
        let end = i;
        while i > 0 && b.get(i - 1).is_some_and(is_ident) {
            i -= 1;
        }
        if i == end {
            break; // no identifier here
        }
        segments.push(prefix.get(i..end)?);
        if i > 0 && b.get(i - 1) == Some(&b'.') {
            i -= 1; // another `.ident` segment to the left
        } else {
            break;
        }
    }
    segments.reverse();
    (!segments.is_empty()).then_some(segments)
}

/// A keyword-argument completion item for a driver/provider schema field. `ctx`
/// is the driver or provider name, shown in the detail line.
/// Whether the cursor (end of `prefix`, the line text before it) sits inside an
/// unclosed string literal. Approximate: counts unescaped double quotes.
fn in_string(prefix: &str) -> bool {
    prefix.bytes().filter(|&b| b == b'"').count() % 2 == 1
}

/// Whether the cursor sits inside the value of a `driver = "…"` keyword argument
/// (`driver = "<partial>` with no closing quote yet on this line).
fn in_driver_value(prefix: &str) -> bool {
    let Some(q) = prefix.rfind('"') else {
        return false;
    };
    // A `"` is ASCII, so `q` is a char boundary.
    let (before, value) = (prefix.get(..q), prefix.get(q + 1..));
    let (Some(before), Some(value)) = (before, value) else {
        return false;
    };
    // No further quote after the last one → still inside the string.
    if value.contains('"') {
        return false;
    }
    let Some(head) = before.trim_end().strip_suffix('=') else {
        return false;
    };
    let head = head.trim_end();
    let Some(stem) = head.strip_suffix("driver") else {
        return false;
    };
    // The token must be exactly `driver`, not a suffix of a longer identifier.
    stem.chars()
        .last()
        .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
}

fn field_item(name: &str, ty: &str, doc: String, ctx: &str, required: bool) -> CompletionItem {
    // Target config fields are mostly optional, so mark the minority — the
    // required ones — explicitly (Bazel `mandatory` / JSON-Schema `required`
    // convention), rather than `?`-marking every optional field.
    let detail = if required {
        format!("{ctx}: {ty} (required)")
    } else {
        format!("{ctx}: {ty}")
    };
    CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(detail),
        documentation: Some(lsp_types::Documentation::String(doc)),
        // Insert as `name = ` to match the keyword-argument call site.
        insert_text: Some(format!("{name} = ")),
        // Editors rank by `sort_text` (falling back to the label), ignoring list
        // order. The `0_` prefix sorts these schema fields ahead of every
        // stock/builtin item (whose effective sort key starts with a letter);
        // the nested `0_`/`1_` floats required fields above optional ones.
        sort_text: Some(format!("0_{}_{name}", if required { 0 } else { 1 })),
        ..Default::default()
    }
}

fn enrich_completion(
    resp: &mut lsp_server::Response,
    uri: &LspUri,
    line: u32,
    col: u32,
    shared: &SharedState,
) {
    let Some(index) = shared.index(uri) else {
        return;
    };
    let (l, c) = (line + 1, col + 1);

    // The source up to the cursor on its line, for string-context detection.
    let line_text = index.source.lines().nth(line as usize).unwrap_or("");
    let prefix = line_text
        .get(..(col as usize).min(line_text.len()))
        .unwrap_or("");

    // Member access on the `heph` namespace (`heph.` → providers, `heph.<provider>.`
    // → that provider's functions) takes priority — the stock server can't
    // complete namespace members at all.
    let member = provider_member_completions(prefix, shared);

    // Inside a `driver = "…"` string → the registered driver names. Otherwise inside
    // a `target(...)` → the driver-independent base args plus (when a driver is
    // chosen) that driver's config fields. Inside a `provider_state(provider="X", …)`
    // → that provider's state fields. Inside any other string we add nothing (the
    // address/string completion path handles those).
    let extra: Vec<CompletionItem> = if !member.is_empty() {
        member
    } else if in_driver_value(prefix) {
        shared
            .engine
            .driver_names()
            .into_iter()
            .map(|name| CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some("driver".to_string()),
                sort_text: Some(format!("0_{name}")),
                ..Default::default()
            })
            .collect()
    } else if in_string(prefix) {
        vec![]
    } else if let Some(driver) = index.driver_at(l, c) {
        let mut items: Vec<CompletionItem> = crate::pluginbuildfile::run_file::target_base_fields()
            .into_iter()
            .map(|f| field_item(&f.name, &f.ty.render(), f.doc, "target", f.required))
            .collect();
        if !driver.is_empty()
            && let Some(schema) = shared.engine.driver_schema(driver)
        {
            items.extend(
                schema
                    .fields
                    .into_iter()
                    .map(|f| field_item(&f.name, &f.ty.render(), f.doc, driver, f.required)),
            );
        }
        items
    } else if let Some(provider) = index.state_provider_at(l, c) {
        shared
            .engine
            .provider_state_schema(provider)
            .map(|s| {
                s.fields
                    .into_iter()
                    .map(|f| field_item(&f.name, &f.ty.render(), f.doc, provider, f.required))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };
    if extra.is_empty() {
        return;
    }

    let mut items: Vec<CompletionItem> = match resp.result.take() {
        Some(v) => match serde_json::from_value::<CompletionResponse>(v) {
            Ok(CompletionResponse::Array(a)) => a,
            Ok(CompletionResponse::List(l)) => l.items,
            Err(_) => vec![],
        },
        None => vec![],
    };
    // Driver fields first, then whatever the stock server proposed.
    let mut merged = extra;
    merged.append(&mut items);
    resp.result = Some(serde_json::to_value(CompletionResponse::Array(merged)).expect("serialize"));
    resp.error = None;
}

#[cfg(test)]
mod tests {
    use super::{find_symbol_def, pkg_of, result_points_to_other_file};
    use std::path::Path;

    #[test]
    fn find_symbol_def_locates_def_and_assignment() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("macros.BUILD");
        std::fs::write(
            &file,
            "# top\n\ndef make_name(p):\n    return p\n\nFOO = 1\nFOOBAR = 2\n",
        )
        .unwrap();

        // `def make_name(` on line index 2; name starts at col 4.
        assert_eq!(find_symbol_def(&file, "make_name"), Some((2, 4, 13)));
        // `FOO = 1` on line 5, col 0..3 — must not match the `FOOBAR` prefix line.
        assert_eq!(find_symbol_def(&file, "FOO"), Some((5, 0, 3)));
        assert_eq!(find_symbol_def(&file, "FOOBAR"), Some((6, 0, 6)));
        // Unknown symbol → None.
        assert_eq!(find_symbol_def(&file, "nope"), None);
    }

    #[test]
    fn in_driver_value_detects_driver_string() {
        use super::{in_driver_value, in_string};
        assert!(in_driver_value(r#"target(name = "t", driver = ""#));
        assert!(in_driver_value(r#"target(name = "t", driver = "ex"#));
        assert!(in_driver_value(r#"  driver="b"#)); // no spaces
        // Not the driver arg.
        assert!(!in_driver_value(r#"target(name = ""#));
        assert!(!in_driver_value(r#"target(mydriver = ""#)); // prefix collision
        // Closed string → not inside it anymore.
        assert!(!in_driver_value(r#"driver = "exec", "#));
        // in_string sanity.
        assert!(in_string(r#"name = "t"#));
        assert!(!in_string(r#"name = "t", "#));
    }

    #[test]
    fn provider_fn_at_resolves_heph_namespace_path() {
        use super::provider_fn_at;
        let line = r#"    srcs = heph.fs.join("a", "b"),"#;
        // `heph.fs.join` starts at col 11; `join` spans cols 19..23.
        let join_col = line.find("join").unwrap();
        // Cursor anywhere within `join` resolves to (provider, fn).
        for c in join_col..=join_col + "join".len() {
            assert_eq!(
                provider_fn_at(line, c),
                Some(("fs".to_string(), "join".to_string())),
                "col {c}"
            );
        }
        // The go namespace resolves too.
        let go = r#"deps = {"golist": [heph.go.build_addr("p", "linux", "amd64")]}"#;
        let c = go.find("build_addr").unwrap() + 2;
        assert_eq!(
            provider_fn_at(go, c),
            Some(("go".to_string(), "build_addr".to_string()))
        );
        // Hovering the namespace segment (`fs`), a bare identifier, or a
        // non-`heph` path yields nothing.
        assert_eq!(provider_fn_at(line, line.find("fs").unwrap()), None);
        assert_eq!(provider_fn_at("    x = join(a, b)", 9), None);
        assert_eq!(provider_fn_at(r#"  y = os.path.join("a")"#, 14), None);
    }

    #[test]
    fn completion_member_base_extracts_dotted_base() {
        use super::completion_member_base;
        // `heph.` → base [heph] (offer namespaces).
        assert_eq!(
            completion_member_base("    srcs = heph."),
            Some(vec!["heph"])
        );
        // partial member after the dot doesn't change the base.
        assert_eq!(completion_member_base("x = heph.f"), Some(vec!["heph"]));
        // two levels → [heph, fs] (offer fs functions).
        assert_eq!(
            completion_member_base("x = heph.fs."),
            Some(vec!["heph", "fs"])
        );
        assert_eq!(
            completion_member_base("x = heph.fs.gl"),
            Some(vec!["heph", "fs"])
        );
        // Not a member access → None.
        assert_eq!(completion_member_base("x = heph"), None);
        assert_eq!(completion_member_base("x = "), None);
        assert_eq!(completion_member_base(""), None);
    }

    #[test]
    fn pkg_of_is_workspace_relative() {
        let root = Path::new("/ws");
        assert_eq!(pkg_of(root, Path::new("/ws/lib/BUILD")), "lib");
        assert_eq!(pkg_of(root, Path::new("/ws/BUILD")), "");
    }

    #[test]
    fn result_points_to_other_file_detects_cross_file_resolution() {
        let doc = Path::new("/ws/app/BUILD");
        // Already resolved into a different file → true (don't override).
        let other = serde_json::json!([{"targetUri": "file:///ws/lib/BUILD"}]);
        assert!(result_points_to_other_file(Some(&other), doc));
        // Resolved to the same doc (the load statement) → false (we should override).
        let same = serde_json::json!([{"targetUri": "file:///ws/app/BUILD"}]);
        assert!(!result_points_to_other_file(Some(&same), doc));
        // Empty result → false.
        assert!(!result_points_to_other_file(
            Some(&serde_json::json!([])),
            doc
        ));
    }
}
