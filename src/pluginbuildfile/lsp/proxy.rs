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
use lsp_server::{Connection, Message, RequestId};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Hover, HoverContents, MarkedString,
    MarkupContent, MarkupKind,
};
use starlark_lsp::server::LspUrl;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// What an intercepted request was, so its response can be enriched.
#[derive(Clone)]
enum Pending {
    Hover { uri: LspUrl, line: u32, col: u32 },
    Completion { uri: LspUrl, line: u32, col: u32 },
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
        _ => None,
    }
}

fn enrich(resp: &mut lsp_server::Response, pending: &Pending, shared: &SharedState) {
    match pending {
        Pending::Hover { uri, line, col } => enrich_hover(resp, uri, *line, *col, shared),
        Pending::Completion { uri, line, col } => enrich_completion(resp, uri, *line, *col, shared),
    }
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
    let Some(addrs) = index.targets_at(line + 1, col + 1) else {
        return;
    };
    if addrs.is_empty() {
        return;
    }

    let mut md = existing_hover_markdown(resp);
    if !md.is_empty() {
        md.push_str("\n\n---\n\n");
    }
    md.push_str(&format!("**Generated targets ({})**\n\n```\n", addrs.len()));
    for a in addrs {
        md.push_str(a);
        md.push('\n');
    }
    md.push_str("```\n");

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
    let Some(driver) = index.driver_at(line + 1, col + 1) else {
        return;
    };
    let Some(schema) = shared.engine.driver_schema(driver) else {
        return;
    };

    let extra: Vec<CompletionItem> = schema
        .fields
        .into_iter()
        .map(|f| CompletionItem {
            label: f.name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(format!("{}: {}", driver, f.ty.render())),
            documentation: Some(lsp_types::Documentation::String(f.doc)),
            // Insert as `name = ` to match the keyword-argument call site.
            insert_text: Some(format!("{} = ", f.name)),
            ..Default::default()
        })
        .collect();
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
