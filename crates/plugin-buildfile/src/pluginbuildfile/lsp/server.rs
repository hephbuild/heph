//! The BUILD-file language server's loop, on `lsp-server`.
//!
//! heph answers `initialize` itself (capabilities declared once, the position
//! encoding negotiated from the client's list), keeps the open documents, and
//! routes each request through the site resolver ([`super::site`]) to the
//! handler table ([`super::handlers`]). The stock answer, where the merge
//! policy passes it through, comes from the vendored analysis modules
//! in-process.
//!
//! One thread handles every message in order. The token array is rebuilt
//! synchronously while a `didChange` is handled, before the next message is
//! read, so a request queued behind an edit always sees that edit's text.

use super::context::HephLspContext;
use super::handlers::{self, Ctx, Merge, Registry};
use super::index::SharedState;
use super::lex::{self, Tokens};
use super::position::{self, Encoding};
use super::site::{self, AstState, Resolution, Site};
use hplugin::lsp::LspEngine;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CompletionItem, CompletionOptions, CompletionParams, CompletionResponse, CompletionTriggerKind,
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, InitializeParams, LocationLink, MarkedString, MarkupContent, MarkupKind, OneOf,
    Position, PublishDiagnosticsParams, Range, ServerCapabilities, ServerInfo, Uri,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use vendored_starlark_lsp::server::{Backend, LspServerSettings, LspUri};
use vendored_starlark_lsp::uri::uri_to_file_path;

/// The completion trigger characters heph declares (D7): `.` for namespace
/// members, quotes for string values, `/` and `:` for addresses.
const TRIGGER_CHARACTERS: [&str; 5] = [".", "\"", "'", "/", ":"];

/// Knobs for [`run_with`]; [`Default`] is what `heph tool build-lsp` uses.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// How long `initialize` waits for the driver/provider registry snapshot
    /// before answering anyway; until it lands, heph answers with the stock
    /// results and `reason: providers_loading`.
    pub registry_wait: Duration,
}

impl Default for RunOptions {
    fn default() -> RunOptions {
        RunOptions {
            registry_wait: Duration::from_secs(5),
        }
    }
}

/// One open document: its text and everything derived from it, freed on
/// `didClose`.
struct Document {
    text: String,
    version: i32,
    tokens: Arc<Tokens>,
    /// The version of the last text that parsed, if any did.
    ast_version: Option<i32>,
    /// Outside the bound repo: stock answers only, `reason: outside_root`.
    outside_root: bool,
}

impl Document {
    fn ast_state(&self) -> AstState {
        match self.ast_version {
            Some(v) if v == self.version => AstState::Current,
            Some(v) => AstState::Stale {
                behind: self.version - v,
            },
            None => AstState::Absent,
        }
    }
}

/// The registry snapshot, loading on its own thread.
enum RegistryState {
    Loading(Receiver<Registry>),
    Ready(Arc<Registry>),
    /// The snapshot thread died; stock answers only.
    Failed,
}

struct Server<'a> {
    connection: &'a Connection,
    backend: Backend<HephLspContext>,
    shared: Arc<SharedState>,
    encoding: Encoding,
    init: InitializeParams,
    /// The repo the engine was bound to; a document outside it gets the stock
    /// answers only.
    root: Root,
    documents: HashMap<LspUri, Document>,
    registry: RegistryState,
    shutting_down: bool,
}

/// The engine's root, as given and canonical (an editor may spell a path
/// either way, `/var` against `/private/var` on macOS).
struct Root {
    path: std::path::PathBuf,
    canonical: Option<std::path::PathBuf>,
}

impl Root {
    fn new(path: &std::path::Path) -> Root {
        Root {
            path: path.to_path_buf(),
            canonical: path.canonicalize().ok(),
        }
    }

    fn contains(&self, p: &std::path::Path) -> bool {
        p.starts_with(&self.path)
            || self.canonical.as_ref().is_some_and(|c| p.starts_with(c))
            || p.canonicalize()
                .is_ok_and(|pc| self.canonical.as_ref().is_some_and(|c| pc.starts_with(c)))
    }
}

/// Serve on `connection` until `exit`, or until the client hangs up.
pub(crate) fn run(connection: Connection, engine: Arc<dyn LspEngine>) -> anyhow::Result<()> {
    run_with(connection, engine, RunOptions::default())
}

/// [`run`] with explicit [`RunOptions`].
pub fn run_with(
    connection: Connection,
    engine: Arc<dyn LspEngine>,
    options: RunOptions,
) -> anyhow::Result<()> {
    // The registry is read once per server start, off the loop's thread.
    let (tx, rx) = std::sync::mpsc::channel();
    let snapshot_engine = Arc::clone(&engine);
    std::thread::spawn(move || {
        // The receiver may be gone if the client already left.
        let _sent = tx.send(Registry::snapshot(&*snapshot_engine));
    });

    let Some((id, params)) = await_initialize(&connection) else {
        return Ok(());
    };
    // One workspace for now: the first folder the editor sends (or its
    // `rootUri`), which must lie in the repo the engine was bound to.
    let folders: Vec<std::path::PathBuf> = match params.get("workspaceFolders") {
        Some(Value::Array(folders)) => folders
            .iter()
            .filter_map(|f| f.get("uri")?.as_str()?.parse::<Uri>().ok())
            .filter_map(|u| uri_to_file_path(&u))
            .collect(),
        _ => params
            .get("rootUri")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Uri>().ok())
            .and_then(|u| uri_to_file_path(&u))
            .into_iter()
            .collect(),
    };
    let root = Root::new(engine.root());
    if let Some(first) = folders.first()
        && !root.contains(first)
    {
        let message = format!(
            "workspace folder {} is outside the heph repo at {}",
            first.display(),
            root.path.display()
        );
        send(
            &connection,
            Response::new_err(id, ErrorCode::InvalidParams as i32, message.clone()),
        );
        return Err(anyhow::anyhow!(message));
    }
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
    let registry = match rx.recv_timeout(options.registry_wait) {
        Ok(r) => RegistryState::Ready(Arc::new(r)),
        Err(RecvTimeoutError::Timeout) => RegistryState::Loading(rx),
        Err(RecvTimeoutError::Disconnected) => RegistryState::Failed,
    };
    let ctx = HephLspContext::new(engine);
    let shared = Arc::clone(&ctx.shared);
    let result = serde_json::json!({
        "capabilities": capabilities(encoding),
        "serverInfo": ServerInfo { name: "heph".to_string(), version: None },
    });
    send(&connection, Response::new_ok(id, result));
    if folders.len() > 1 {
        let message = format!(
            "heph's BUILD-file server supports one workspace folder; using {} and ignoring the other {}",
            folders
                .first()
                .map_or_else(String::new, |f| f.display().to_string()),
            folders.len() - 1
        );
        let _sent = connection
            .sender
            .send(Message::Notification(Notification::new(
                "window/showMessage".to_string(),
                lsp_types::ShowMessageParams {
                    typ: lsp_types::MessageType::WARNING,
                    message,
                },
            )));
    }

    let mut server = Server {
        connection: &connection,
        backend: Backend::new(ctx),
        shared,
        encoding,
        init,
        root,
        documents: HashMap::new(),
        registry,
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
        // Document symbols stay off until implemented.
        document_symbol_provider: None,
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

/// The stock hover's text, whichever shape it came in.
fn hover_text(hover: &Hover) -> String {
    let marked = |m: &MarkedString| match m {
        MarkedString::String(s) => s.clone(),
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    };
    match &hover.contents {
        HoverContents::Markup(m) => m.value.clone(),
        HoverContents::Scalar(s) => marked(s),
        HoverContents::Array(items) => items.iter().map(marked).collect::<Vec<_>>().join("\n\n"),
    }
}

/// Whether a stock hover is nothing but headings. The stock renderer gives an
/// undocumented global (`heph`) a level-one heading of its name and nothing
/// else, and answers anywhere on a dotted chain with its root's hover, so the
/// whole of `heph.fs.base(f).removesuffix(…)` showed a big `heph`.
fn is_heading_only(md: &str) -> bool {
    md.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .all(|l| l.starts_with('#'))
}

/// A located request: the document, the cursor's byte offset, and its
/// resolution.
struct Located {
    key: LspUri,
    offset: usize,
    resolution: Resolution,
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
            "textDocument/formatting" => Ok(self.formatting(&req.params)),
            #[cfg(feature = "test-support")]
            "heph/documents" => Ok(serde_json::json!({
                "documents": self.documents.len(),
                "parses": self.backend.parsed_documents(),
                "indexes": self.shared.len(),
            })),
            #[cfg(feature = "test-support")]
            "heph/explain" => parse::<lsp_types::TextDocumentPositionParams>(&req).map(|p| {
                self.locate(&p.text_document.uri, p.position)
                    .map_or(Value::Null, |l| l.resolution.to_json())
            }),
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
        // Lexed once per edit: the parse below (and its diagnostics) reads the
        // same token array the requests will.
        let tokens = Arc::new(lex::lex(&text));
        self.shared.stage_tokens(key.clone(), Arc::clone(&tokens));
        let (stock, parsed) = self.backend.validate(&key, text.clone());
        let ast_version = if parsed {
            Some(version)
        } else {
            self.documents.get(&key).and_then(|d| d.ast_version)
        };
        let mut diagnostics: Vec<Diagnostic> = stock
            .into_iter()
            .map(|mut d| {
                d.range = self.stock_range(d.range, &text);
                d
            })
            .collect();
        // Every lexer error too, not only the parser's first: the root cause
        // of each line.
        for err in tokens.errors.iter().filter(|e| e.publish) {
            let Some(t) = tokens.toks.get(err.token as usize) else {
                continue;
            };
            let range = Range::new(
                position::to_position(&text, t.start as usize, self.encoding),
                position::to_position(&text, t.end as usize, self.encoding),
            );
            if diagnostics.iter().any(|d| d.range.start == range.start) {
                continue;
            }
            diagnostics.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("starlark".to_string()),
                message: err.message.clone(),
                ..Default::default()
            });
        }
        self.publish(client_uri, diagnostics, Some(version));
        let outside_root = match self.documents.get(&key) {
            Some(prev) => prev.outside_root,
            None => matches!(&key, LspUri::File(p) if !self.root.contains(p)),
        };
        self.documents.insert(
            key,
            Document {
                text,
                version,
                tokens,
                ast_version,
                outside_root,
            },
        );
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

    fn publish(&self, uri: Uri, diagnostics: Vec<Diagnostic>, version: Option<i32>) {
        let params = PublishDiagnosticsParams::new(uri, diagnostics, version);
        let _sent = self
            .connection
            .sender
            .send(Message::Notification(Notification::new(
                "textDocument/publishDiagnostics".to_string(),
                params,
            )));
    }

    /// The registry, once its snapshot has landed.
    fn registry(&mut self) -> Option<Arc<Registry>> {
        if let RegistryState::Loading(rx) = &self.registry {
            match rx.try_recv() {
                Ok(r) => self.registry = RegistryState::Ready(Arc::new(r)),
                Err(std::sync::mpsc::TryRecvError::Empty) => return None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.registry = RegistryState::Failed;
                }
            }
        }
        match &self.registry {
            RegistryState::Ready(r) => Some(Arc::clone(r)),
            _ => None,
        }
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

    /// Resolve a request position. `None` when the document is unknown or the
    /// position lies past its end: not a site, so the caller answers stock.
    fn locate(&mut self, uri: &Uri, pos: Position) -> Option<Located> {
        let key = uri_key(uri)?;
        let ready = self.registry().is_some();
        let doc = self.documents.get(&key)?;
        let offset = position::to_offset(&doc.text, pos, self.encoding)?;
        let overlay = self.shared.overlay(&key);
        let resolution = site::resolve(
            &site::Doc {
                text: &doc.text,
                tokens: &doc.tokens,
                overlay: overlay.as_deref(),
                ast: doc.ast_state(),
            },
            offset,
        );
        let resolution = if doc.outside_root {
            Resolution::none(site::Reason::OutsideRoot)
        } else if !ready && !matches!(resolution.site, Site::None | Site::ArgKey { .. }) {
            Resolution::none(site::Reason::ProvidersLoading)
        } else {
            resolution
        };
        Some(Located {
            key,
            offset,
            resolution,
        })
    }

    fn log(&self, method: &str, located: &Located, answered: usize, started: Instant) {
        tracing::debug!(
            method,
            site = located.resolution.site.kind(),
            reason = ?located.resolution.reason,
            degraded = ?located.resolution.degraded,
            answered,
            elapsed_us = started.elapsed().as_micros() as u64,
            "lsp: resolved"
        );
    }

    /// Run `f` with a handler context for the located document.
    fn with_ctx<T>(
        &self,
        key: &LspUri,
        registry: &Registry,
        f: impl FnOnce(&Ctx<'_>) -> T,
    ) -> Option<T> {
        let doc = self.documents.get(key)?;
        Some(f(&Ctx {
            text: &doc.text,
            tokens: &doc.tokens,
            uri: key,
            encoding: self.encoding,
            registry,
            shared: &self.shared,
            listing: &self.backend.context,
        }))
    }

    /// The `(line, byte column)` the stock analysis indexes by.
    fn stock_position(&self, key: &LspUri, offset: usize) -> Position {
        self.documents.get(key).map_or(Position::new(0, 0), |d| {
            position::newline_position(&d.text, offset)
        })
    }

    fn completion(&mut self, params: CompletionParams) -> Value {
        let started = Instant::now();
        let uri = params.text_document_position.text_document.uri.clone();
        let Some(located) = self.locate(&uri, params.text_document_position.position) else {
            return serde_json::json!([]);
        };
        // A trigger character on a non-site returns at once, without the
        // handlers or the engine: `:` only completes in an address-shaped
        // string, `/` only inside a string.
        if let Some(ctx) = &params.context
            && ctx.trigger_kind == CompletionTriggerKind::TRIGGER_CHARACTER
            && let Some(doc) = self.documents.get(&located.key)
        {
            let sdoc = site::Doc {
                text: &doc.text,
                tokens: &doc.tokens,
                overlay: None,
                ast: AstState::Current,
            };
            let fires = match ctx.trigger_character.as_deref() {
                Some(":") => site::colon_is_address(&sdoc, located.offset),
                Some("/") => site::slash_in_string(&sdoc, located.offset),
                _ => true,
            };
            if !fires {
                self.log("completion", &located, 0, started);
                return serde_json::json!([]);
            }
        }
        let items: Vec<CompletionItem> = match handlers::merge_for(&located.resolution.site) {
            Merge::PassThrough => self.stock_completion(params, &located),
            Merge::Prepend => {
                let stock = self.stock_completion(params, &located);
                // Parameters need no registry: they come from the document,
                // the loaded files and the builtins' docs.
                let empty = Registry::default();
                self.with_ctx(&located.key, &empty, |ctx| {
                    handlers::prepend(&located.resolution.site, ctx, stock)
                })
                .unwrap_or_default()
            }
            Merge::Replace => match self.registry() {
                Some(registry) => self
                    .with_ctx(&located.key, &registry, |ctx| {
                        handlers::items(&located.resolution.site, ctx)
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            },
        };
        self.log("completion", &located, items.len(), started);
        serde_json::to_value(items).unwrap_or_else(|_| serde_json::json!([]))
    }

    fn stock_completion(
        &self,
        mut params: CompletionParams,
        located: &Located,
    ) -> Vec<CompletionItem> {
        params.text_document_position.position = self.stock_position(&located.key, located.offset);
        let mut items = match self.backend.completion_options(params, &self.init) {
            Ok(CompletionResponse::Array(items)) => items,
            Ok(CompletionResponse::List(list)) => list.items,
            Err(e) => {
                tracing::debug!(error = e.format(), "lsp: stock completion failed");
                Vec::new()
            }
        };
        if let Some(text) = self.text_of(&located.key) {
            for item in &mut items {
                if let Some(lsp_types::CompletionTextEdit::Edit(edit)) = item.text_edit.as_mut() {
                    edit.range = self.stock_range(edit.range, &text);
                }
                for edit in item.additional_text_edits.iter_mut().flatten() {
                    edit.range = self.stock_range(edit.range, &text);
                }
            }
        }
        items
    }

    fn hover(&mut self, params: HoverParams) -> Value {
        let started = Instant::now();
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some(located) = self.locate(&uri, params.text_document_position_params.position) else {
            return Value::Null;
        };
        let site = &located.resolution.site;
        let registry = self.registry().unwrap_or_default();
        let reg = &*registry;
        let hover = match site {
            Site::None | Site::ArgKey { .. } => self.stock_hover(params, &located, reg),
            _ => self
                .with_ctx(&located.key, reg, |ctx| {
                    let mut hover = handlers::hover(site, ctx)?;
                    if let Site::Callee { .. } = site
                        && let Some(block) = handlers::provenance_block(ctx, located.offset)
                        && let HoverContents::Markup(m) = &mut hover.contents
                    {
                        m.value.push_str("\n\n---\n\n");
                        m.value.push_str(&block);
                    }
                    Some(hover)
                })
                .flatten(),
        };
        self.log("hover", &located, usize::from(hover.is_some()), started);
        hover.map_or(Value::Null, |h| {
            serde_json::to_value(h).unwrap_or(Value::Null)
        })
    }

    /// The stock hover, with the evaluation's enrichments: an undocumented
    /// local `def`'s signature when the stock has nothing, and the targets the
    /// call site produced.
    fn stock_hover(
        &self,
        mut params: HoverParams,
        located: &Located,
        reg: &Registry,
    ) -> Option<Hover> {
        params.text_document_position_params.position =
            self.stock_position(&located.key, located.offset);
        let stock = self.backend.hover_info(params, &self.init).ok();
        let range = stock.as_ref().and_then(|h| h.range).and_then(|r| {
            self.text_of(&located.key)
                .map(|text| self.stock_range(r, &text))
        });
        let mut md = stock
            .as_ref()
            .map(hover_text)
            .filter(|md| !is_heading_only(md))
            .unwrap_or_default();
        self.with_ctx(&located.key, reg, |ctx| {
            if md.is_empty()
                && let Some(doc) = handlers::def_fallback(ctx, located.offset)
            {
                md.push_str(&doc);
            }
            if let Some(block) = handlers::provenance_block(ctx, located.offset) {
                if !md.is_empty() {
                    md.push_str("\n\n---\n\n");
                }
                md.push_str(&block);
            }
        });
        if md.is_empty() {
            return None;
        }
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: md,
            }),
            range,
        })
    }

    fn definition(&mut self, params: GotoDefinitionParams) -> Value {
        let started = Instant::now();
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some(located) = self.locate(&uri, params.text_document_position_params.position) else {
            return serde_json::json!([]);
        };
        let site = located.resolution.site.clone();
        let empty = Registry::default();
        let heph = self
            .with_ctx(&located.key, &empty, |ctx| handlers::definition(&site, ctx))
            .flatten();
        let links = match heph {
            Some(links) => links,
            None => self.stock_definition(params, &located),
        };
        self.log("definition", &located, links.len(), started);
        serde_json::to_value(links).unwrap_or_else(|_| serde_json::json!([]))
    }

    /// The stock definition; a loaded name it could only resolve to its
    /// `load` line (or not at all) jumps to its `def` in whichever of the
    /// package's files defines it.
    fn stock_definition(
        &self,
        mut params: GotoDefinitionParams,
        located: &Located,
    ) -> Vec<LocationLink> {
        params.text_document_position_params.position =
            self.stock_position(&located.key, located.offset);
        let mut links = match self.backend.find_definition(params, &self.init) {
            Ok(GotoDefinitionResponse::Link(links)) => links,
            Ok(_) => Vec::new(),
            Err(e) => {
                tracing::debug!(error = e.format(), "lsp: stock definition failed");
                Vec::new()
            }
        };
        for link in &mut links {
            if let (Some(range), Some(text)) =
                (link.origin_selection_range, self.text_of(&located.key))
            {
                link.origin_selection_range = Some(self.stock_range(range, &text));
            }
            if let Some(target) = uri_key(&link.target_uri)
                && let Some(text) = self.text_of(&target)
            {
                link.target_range = self.stock_range(link.target_range, &text);
                link.target_selection_range = self.stock_range(link.target_selection_range, &text);
            }
        }
        let elsewhere = links
            .iter()
            .any(|l| uri_key(&l.target_uri).as_ref() != Some(&located.key));
        if elsewhere {
            return links;
        }
        let Some(doc) = self.documents.get(&located.key) else {
            return links;
        };
        let word = doc
            .tokens
            .toks
            .iter()
            .find(|t| {
                t.kind == lex::Kind::Ident
                    && t.start as usize <= located.offset
                    && located.offset <= t.end as usize
            })
            .map(lex::Tok::range);
        let loaded = self
            .shared
            .overlay(&located.key)
            .map(|o| o.loaded.clone())
            .unwrap_or_default();
        let empty = Registry::default();
        let jump = word.and_then(|w| {
            self.with_ctx(&located.key, &empty, |ctx| {
                handlers::loaded_definition(ctx, w, &loaded)
            })
            .flatten()
        });
        match jump {
            Some(link) => vec![link],
            None => links,
        }
    }

    fn formatting(&self, params: &Value) -> Value {
        let key = params
            .pointer("/textDocument/uri")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Uri>().ok())
            .and_then(|u| uri_key(&u));
        let Some(key) = key else {
            return Value::Null;
        };
        let Some(doc) = self.documents.get(&key) else {
            return Value::Null;
        };
        let name = key.path().to_string_lossy().into_owned();
        handlers::format(&name, &doc.text, &*self.shared.engine, self.encoding)
    }
}
