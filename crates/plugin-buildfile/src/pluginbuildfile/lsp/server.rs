//! The BUILD-file language server's loop, on `lsp-server`.
//!
//! heph answers `initialize` itself (capabilities declared once, the position
//! encoding negotiated from the client's list), keeps the open documents, and
//! routes each request: the stock answer comes from the vendored analysis
//! modules in-process, then heph's enrichment is applied to it. One thread
//! handles every message in order, so a request queued behind an edit always
//! sees that edit's text.

use super::context::HephLspContext;
use super::index::SharedState;
use super::position::{self, Encoding};
use super::proxy;
use hplugin::lsp::LspEngine;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, InitializeParams, OneOf, Position,
    PublishDiagnosticsParams, Range, ServerCapabilities, ServerInfo, Uri,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use vendored_starlark_lsp::server::{Backend, LspServerSettings, LspUri};

/// The completion trigger characters heph declares (D7): `.` for namespace
/// members, quotes for string values, `/` and `:` for addresses.
const TRIGGER_CHARACTERS: [&str; 5] = [".", "\"", "'", "/", ":"];

/// One open document.
struct Document {
    text: String,
}

struct Server<'a> {
    connection: &'a Connection,
    backend: Backend<HephLspContext>,
    shared: Arc<SharedState>,
    encoding: Encoding,
    init: InitializeParams,
    documents: HashMap<LspUri, Document>,
    shutting_down: bool,
}

/// Serve on `connection` until `exit`, or until the client hangs up.
pub(crate) fn run(connection: Connection, engine: Arc<dyn LspEngine>) -> anyhow::Result<()> {
    let Some((id, params)) = await_initialize(&connection) else {
        return Ok(());
    };
    let init: InitializeParams = match serde_json::from_value(params) {
        Ok(init) => init,
        Err(e) => {
            send(
                &connection,
                Response::new_err(
                    id,
                    ErrorCode::InvalidParams as i32,
                    format!("invalid initialize params: {e}"),
                ),
            );
            return Err(anyhow::anyhow!("invalid initialize params: {e}"));
        }
    };
    let encoding = Encoding::negotiate(
        init.capabilities
            .general
            .as_ref()
            .and_then(|g| g.position_encodings.as_deref()),
    );
    let ctx = HephLspContext::new(engine);
    let shared = Arc::clone(&ctx.shared);
    let result = serde_json::json!({
        "capabilities": capabilities(encoding),
        "serverInfo": ServerInfo { name: "heph".to_string(), version: None },
    });
    send(&connection, Response::new_ok(id, result));

    let mut server = Server {
        connection: &connection,
        backend: Backend::new(ctx),
        shared,
        encoding,
        init,
        documents: HashMap::new(),
        shutting_down: false,
    };
    server.main_loop();
    Ok(())
}

/// Wait for `initialize`, answering every other request with
/// `ServerNotInitialized`. `None` when the client exits or hangs up first.
fn await_initialize(connection: &Connection) -> Option<(RequestId, Value)> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) if req.method == "initialize" => {
                return Some((req.id, req.params));
            }
            Message::Request(req) => send(
                connection,
                Response::new_err(
                    req.id,
                    ErrorCode::ServerNotInitialized as i32,
                    format!("`{}` before `initialize`", req.method),
                ),
            ),
            Message::Notification(n) if n.method == "exit" => return None,
            Message::Notification(_) | Message::Response(_) => {}
        }
    }
    None
}

/// The capabilities heph declares: the stock analysis's, plus formatting and
/// the trigger characters, with the negotiated position encoding.
fn capabilities(encoding: Encoding) -> ServerCapabilities {
    let stock = Backend::<HephLspContext>::server_capabilities(LspServerSettings::default());
    let mut triggers = stock
        .completion_provider
        .as_ref()
        .and_then(|c| c.trigger_characters.clone())
        .unwrap_or_default();
    for t in TRIGGER_CHARACTERS {
        if !triggers.iter().any(|have| have == t) {
            triggers.push(t.to_string());
        }
    }
    ServerCapabilities {
        position_encoding: Some(encoding.kind()),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(triggers),
            ..stock.completion_provider.clone().unwrap_or_default()
        }),
        document_formatting_provider: Some(OneOf::Left(true)),
        ..stock
    }
}

fn send(connection: &Connection, resp: Response) {
    // The client hung up: the loop sees the closed channel next and returns.
    let _sent = connection.sender.send(Message::Response(resp));
}

fn parse<T: DeserializeOwned>(req: &Request) -> Result<T, Response> {
    serde_json::from_value(req.params.clone()).map_err(|e| {
        Response::new_err(
            req.id.clone(),
            ErrorCode::InvalidParams as i32,
            format!("invalid params for `{}`: {e}", req.method),
        )
    })
}

fn uri_key(uri: &Uri) -> Option<LspUri> {
    LspUri::try_from(uri.clone()).ok()
}

impl Server<'_> {
    fn main_loop(&mut self) {
        while let Ok(msg) = self.connection.receiver.recv() {
            match msg {
                Message::Request(req) => {
                    let resp = self.request(req);
                    send(self.connection, resp);
                }
                Message::Notification(n) => {
                    if n.method == "exit" {
                        return;
                    }
                    self.notification(n);
                }
                Message::Response(_) => {}
            }
        }
    }

    fn request(&mut self, req: Request) -> Response {
        if self.shutting_down {
            return Response::new_err(
                req.id,
                ErrorCode::InvalidRequest as i32,
                format!("`{}` after `shutdown`", req.method),
            );
        }
        let id = req.id.clone();
        let result = match req.method.as_str() {
            "shutdown" => {
                self.shutting_down = true;
                Ok(Value::Null)
            }
            "textDocument/completion" => parse(&req).map(|p| self.completion(p)),
            "textDocument/hover" => parse(&req).map(|p| self.hover(p)),
            "textDocument/definition" => parse(&req).map(|p| self.definition(p)),
            "textDocument/formatting" => Ok(proxy::format_request(&req.params, &self.shared)),
            #[cfg(feature = "test-support")]
            "heph/documents" => Ok(serde_json::json!({
                "documents": self.documents.len(),
                "parses": self.backend.parsed_documents(),
                "indexes": self.shared.len(),
            })),
            other => Err(Response::new_err(
                req.id,
                ErrorCode::MethodNotFound as i32,
                format!("unhandled method `{other}`"),
            )),
        };
        match result {
            Ok(value) => Response::new_ok(id, value),
            Err(resp) => resp,
        }
    }

    fn notification(&mut self, n: Notification) {
        match n.method.as_str() {
            "textDocument/didOpen" => {
                if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) {
                    let doc = p.text_document;
                    self.update(doc.uri, doc.text, doc.version);
                }
            }
            "textDocument/didChange" => {
                if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params)
                    && let Some(change) = p.content_changes.into_iter().next_back()
                {
                    // Full sync: the last change carries the whole text.
                    self.update(p.text_document.uri, change.text, p.text_document.version);
                }
            }
            "textDocument/didClose" => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) {
                    self.close(p.text_document.uri);
                }
            }
            // Every request is answered before the next message is read, so
            // a cancellation always names a request that is already done.
            "$/cancelRequest" => {}
            _ => {}
        }
    }

    fn update(&mut self, client_uri: Uri, text: String, version: i32) {
        let Some(key) = uri_key(&client_uri) else {
            return;
        };
        let diagnostics = self.backend.validate(&key, text.clone());
        let diagnostics = diagnostics
            .into_iter()
            .map(|mut d| {
                d.range = self.stock_range(d.range, &text);
                d
            })
            .collect();
        self.publish(client_uri, diagnostics, Some(version));
        self.documents.insert(key, Document { text });
    }

    fn close(&mut self, client_uri: Uri) {
        let Some(key) = uri_key(&client_uri) else {
            return;
        };
        self.documents.remove(&key);
        self.backend.close(&key);
        self.shared.remove(&key);
        self.publish(client_uri, Vec::new(), None);
    }

    fn publish(&self, uri: Uri, diagnostics: Vec<lsp_types::Diagnostic>, version: Option<i32>) {
        let params = PublishDiagnosticsParams::new(uri, diagnostics, version);
        let _sent = self
            .connection
            .sender
            .send(Message::Notification(Notification::new(
                "textDocument/publishDiagnostics".to_string(),
                params,
            )));
    }

    /// The text of `uri`: the open document's, else the file on disk.
    fn text_of(&self, uri: &LspUri) -> Option<Cow<'_, str>> {
        match self.documents.get(uri) {
            Some(doc) => Some(Cow::Borrowed(doc.text.as_str())),
            None => match uri {
                LspUri::File(path) => std::fs::read_to_string(path).ok().map(Cow::Owned),
                _ => None,
            },
        }
    }

    /// A stock range (codemap lines, character columns) in the negotiated
    /// encoding, against `text`.
    fn stock_range(&self, range: Range, text: &str) -> Range {
        Range::new(
            position::from_codemap(text, range.start, self.encoding),
            position::from_codemap(text, range.end, self.encoding),
        )
    }

    /// The `(uri, newline line, byte column)` of a request position, or `None`
    /// when the document is unknown or the position lies past its end.
    fn locate(&self, uri: &Uri, pos: Position) -> Option<(LspUri, Position)> {
        let key = uri_key(uri)?;
        let doc = self.documents.get(&key)?;
        let offset = position::to_offset(&doc.text, pos, self.encoding)?;
        Some((key, position::newline_position(&doc.text, offset)))
    }

    fn completion(&mut self, mut params: CompletionParams) -> Value {
        let uri = params.text_document_position.text_document.uri.clone();
        let Some((key, at)) = self.locate(&uri, params.text_document_position.position) else {
            return serde_json::json!([]);
        };
        params.text_document_position.position = at;
        let stock = match self.backend.completion_options(params, &self.init) {
            Ok(CompletionResponse::Array(mut items)) => {
                if let Some(text) = self.text_of(&key) {
                    for item in &mut items {
                        if let Some(lsp_types::CompletionTextEdit::Edit(edit)) =
                            item.text_edit.as_mut()
                        {
                            edit.range = self.stock_range(edit.range, &text);
                        }
                        for edit in item.additional_text_edits.iter_mut().flatten() {
                            edit.range = self.stock_range(edit.range, &text);
                        }
                    }
                }
                CompletionResponse::Array(items)
            }
            Ok(other) => other,
            Err(e) => {
                tracing::debug!(error = e.format(), "lsp: stock completion failed");
                CompletionResponse::Array(Vec::new())
            }
        };
        let mut resp = Response::new_ok(RequestId::from(0), stock);
        proxy::enrich_completion(&mut resp, &key, at.line, at.character, &self.shared);
        resp.result.unwrap_or(Value::Null)
    }

    fn hover(&mut self, mut params: HoverParams) -> Value {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some((key, at)) = self.locate(&uri, params.text_document_position_params.position)
        else {
            return Value::Null;
        };
        params.text_document_position_params.position = at;
        let stock = match self.backend.hover_info(params, &self.init) {
            Ok(mut hover) => {
                if let (Some(range), Some(text)) = (hover.range, self.text_of(&key)) {
                    hover.range = Some(self.stock_range(range, &text));
                }
                serde_json::to_value(hover).unwrap_or(Value::Null)
            }
            Err(e) => {
                tracing::debug!(error = e.format(), "lsp: stock hover failed");
                Value::Null
            }
        };
        let mut resp = Response::new_ok(RequestId::from(0), stock);
        proxy::enrich_hover(&mut resp, &key, at.line, at.character, &self.shared);
        resp.result.unwrap_or(Value::Null)
    }

    fn definition(&mut self, mut params: GotoDefinitionParams) -> Value {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some((key, at)) = self.locate(&uri, params.text_document_position_params.position)
        else {
            return serde_json::json!([]);
        };
        params.text_document_position_params.position = at;
        let stock = match self.backend.find_definition(params, &self.init) {
            Ok(GotoDefinitionResponse::Link(mut links)) => {
                for link in &mut links {
                    if let (Some(range), Some(text)) =
                        (link.origin_selection_range, self.text_of(&key))
                    {
                        link.origin_selection_range = Some(self.stock_range(range, &text));
                    }
                    if let Some(target) = uri_key(&link.target_uri)
                        && let Some(text) = self.text_of(&target)
                    {
                        link.target_range = self.stock_range(link.target_range, &text);
                        link.target_selection_range =
                            self.stock_range(link.target_selection_range, &text);
                    }
                }
                GotoDefinitionResponse::Link(links)
            }
            Ok(other) => other,
            Err(e) => {
                tracing::debug!(error = e.format(), "lsp: stock definition failed");
                GotoDefinitionResponse::Link(Vec::new())
            }
        };
        let mut resp = Response::new_ok(RequestId::from(0), stock);
        proxy::enrich_definition(&mut resp, &key, at.line, at.character, &self.shared);
        resp.result.unwrap_or(Value::Null)
    }
}
