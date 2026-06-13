//! A transparent middleware that sits between the editor (stdio) and the
//! `starlark_lsp` server (an in-memory connection). It forwards every message
//! untouched except `textDocument/hover` and `textDocument/completion`: those are
//! forwarded too, but their responses are enriched on the way back with heph
//! specifics the stock server can't know about —
//! - hover gains a "Generated targets" block (the addresses produced by the
//!   symbol under the cursor), and
//! - completion gains the config fields of the target's `driver` (from the
//!   driver's [`schema`](crate::engine::driver::Driver::schema)).

use super::index::SharedState;
use crate::pluginbuildfile::run_file::resolve_load_target;
use lsp_server::{Connection, Message, Notification, RequestId};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Hover, HoverContents, LocationLink,
    MarkedString, MarkupContent, MarkupKind, Position, Range,
};
use starlark_lsp::server::LspUrl;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What an intercepted request was, so its response can be enriched.
#[derive(Clone)]
enum Pending {
    Hover { uri: LspUrl, line: u32, col: u32 },
    Completion { uri: LspUrl, line: u32, col: u32 },
    Definition { uri: LspUrl, line: u32, col: u32 },
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
    let pos = params.get("position")?;
    let line = pos.get("line")?.as_u64()? as u32;
    let col = pos.get("character")?.as_u64()? as u32;
    let uri_str = params.get("textDocument")?.get("uri")?.as_str()?;
    let uri = LspUrl::try_from(lsp_types::Url::parse(uri_str).ok()?).ok()?;
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
    }
}

/// Resolve goto-definition for a `load`-imported symbol across every BUILD file
/// in its package. The stock server only parses one file per package, so a symbol
/// defined in a sibling file resolves to the `load(...)` line (or nothing); we
/// scan the whole package and point at the real definition.
fn enrich_definition(
    resp: &mut lsp_server::Response,
    uri: &LspUrl,
    line: u32,
    col: u32,
    shared: &SharedState,
) {
    let LspUrl::File(doc_path) = uri else {
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
            let Ok(url) = lsp_types::Url::from_file_path(&file) else {
                continue;
            };
            let target_range = Range {
                start: Position::new(dl, c0),
                end: Position::new(dl, c1),
            };
            let link = LocationLink {
                origin_selection_range: None,
                target_uri: url,
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
    let doc = lsp_types::Url::from_file_path(doc_path).ok();
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
    uri: &LspUrl,
    line: u32,
    col: u32,
    shared: &SharedState,
) {
    let Some(index) = shared.index(uri) else {
        return;
    };

    // Index positions are 1-based.
    let mut md = existing_hover_markdown(resp);

    // If the stock server produced no hover (e.g. an undocumented `def`), fall
    // back to the function's rendered signature.
    if md.is_empty()
        && let Some(doc) = index.def_hover_at(line + 1, col + 1)
    {
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
        md.push_str(&format!(
            "**Generated targets ({})**\n\n```\n",
            targets.len()
        ));
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

/// A keyword-argument completion item for a driver/provider schema field. `ctx`
/// is the driver or provider name, shown in the detail line.
fn field_item(name: &str, ty: &str, doc: String, ctx: &str) -> CompletionItem {
    CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(format!("{ctx}: {ty}")),
        documentation: Some(lsp_types::Documentation::String(doc)),
        // Insert as `name = ` to match the keyword-argument call site.
        insert_text: Some(format!("{name} = ")),
        ..Default::default()
    }
}

fn enrich_completion(
    resp: &mut lsp_server::Response,
    uri: &LspUrl,
    line: u32,
    col: u32,
    shared: &SharedState,
) {
    let Some(index) = shared.index(uri) else {
        return;
    };
    let (l, c) = (line + 1, col + 1);

    // Inside a `target(driver="X", …)` → that driver's config fields; inside a
    // `provider_state(provider="X", …)` → that provider's state fields.
    let extra: Vec<CompletionItem> = if let Some(driver) = index.driver_at(l, c) {
        shared
            .engine
            .driver_schema(driver)
            .map(|s| {
                s.fields
                    .into_iter()
                    .map(|f| field_item(&f.name, &f.ty.render(), f.doc, driver))
                    .collect()
            })
            .unwrap_or_default()
    } else if let Some(provider) = index.state_provider_at(l, c) {
        shared
            .engine
            .provider_state_schema(provider)
            .map(|s| {
                s.fields
                    .into_iter()
                    .map(|f| field_item(&f.name, &f.ty.render(), f.doc, provider))
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
