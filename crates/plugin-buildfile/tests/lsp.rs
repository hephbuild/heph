//! Editor-shaped end-to-end tests for the BUILD-file language server.
//!
//! Each test starts the real server over an in-memory connection, sends
//! `initialize` and `didOpen` exactly as an editor would, then a request at a
//! position, and asserts on the JSON the editor receives. Fixtures mark the
//! cursor with `│`, so no column is ever counted by hand; every receive is a
//! bounded `recv_timeout`, so a hang fails the test instead of stalling CI.
//!
//! The registry is a [`FakeEngine`]: two drivers with distinct fields, one
//! provider with a state schema, and one plugin function.

use hcore::htvalue::signature::{FnSignature, Param, ParamType};
use hplugin::driver::{DriverField, DriverSchema};
use hplugin::function::{
    FnArgs, FnCallContext, FnOutcome, FunctionRegistry, PluginFn, PluginFnDef,
};
use hplugin::lsp::LspEngine;
use hplugin::provider::{StateField, StateSchema};
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How long any single receive may take before the test fails as a hang.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The cursor marker in fixtures.
const CURSOR: char = '│';

// ---------------------------------------------------------------------------
// The registry.
// ---------------------------------------------------------------------------

struct FakeEngine {
    root: PathBuf,
    registry: Arc<FunctionRegistry>,
}

struct JoinFn;

#[async_trait::async_trait]
impl PluginFn for JoinFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        let parts: Vec<String> = args
            .positional
            .iter()
            .filter_map(|v| match v {
                hcore::htvalue::Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        Ok(hcore::htvalue::Value::String(parts.join("/")).into())
    }
}

fn registry() -> Arc<FunctionRegistry> {
    let mut registry = FunctionRegistry::default();
    registry
        .insert(
            "fs",
            vec![PluginFnDef {
                name: "join".to_string(),
                signature: FnSignature {
                    positional: vec![
                        Param::required("a", ParamType::String),
                        Param::required("b", ParamType::String),
                    ],
                    named: vec![],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: "Join two path segments.".to_string(),
                func: Arc::new(JoinFn),
            }],
        )
        .expect("insert fs.join");
    Arc::new(registry)
}

fn field(name: &str, ty: ParamType, doc: &str, required: bool) -> DriverField {
    DriverField {
        name: name.to_string(),
        ty,
        doc: doc.to_string(),
        required,
    }
}

impl LspEngine for FakeEngine {
    fn root(&self) -> &Path {
        &self.root
    }
    fn function_registry(&self) -> Arc<FunctionRegistry> {
        Arc::clone(&self.registry)
    }
    fn driver_schema(&self, name: &str) -> Option<DriverSchema> {
        match name {
            "exec" => Some(DriverSchema {
                accepts_deferred: false,
                fields: vec![
                    field("cmd", ParamType::String, "Command line to run.", true),
                    field(
                        "env",
                        ParamType::map(ParamType::String),
                        "Environment variables.",
                        false,
                    ),
                ],
            }),
            "go_binary" => Some(DriverSchema {
                accepts_deferred: false,
                fields: vec![
                    field("pkg", ParamType::String, "Go package to build.", true),
                    field(
                        "ldflags",
                        ParamType::list(ParamType::String),
                        "Linker flags.",
                        false,
                    ),
                ],
            }),
            _ => None,
        }
    }
    fn driver_names(&self) -> Vec<String> {
        vec!["exec".to_string(), "go_binary".to_string()]
    }
    fn provider_state_schema(&self, name: &str) -> Option<StateSchema> {
        (name == "go").then(|| StateSchema {
            fields: vec![StateField {
                name: "go_codegen_root".to_string(),
                ty: ParamType::Bool,
                doc: "Mark this package as a Go codegen root.".to_string(),
                required: false,
            }],
        })
    }
    fn provider_options(&self, _name: &str) -> hplugin::config::Options {
        hplugin::config::Options::default()
    }
}

// ---------------------------------------------------------------------------
// The harness.
// ---------------------------------------------------------------------------

/// An open document and the cursor position its fixture marked.
#[derive(Clone, Debug)]
struct Doc {
    uri: String,
    line: u32,
    character: u32,
    version: i32,
}

impl Doc {
    fn position(&self) -> Value {
        json!({"line": self.line, "character": self.character})
    }
}

/// Strip the `│` marker and return the text plus its 0-based (line, UTF-16
/// column). Without a marker the cursor is at the end of the text.
/// The column is counted in the named position encoding; lines end at `\n`,
/// `\r\n` or a lone `\r`, as the LSP defines them.
fn split_cursor_in(fixture: &str, encoding: &str) -> (String, u32, u32) {
    let (before, after) = match fixture.split_once(CURSOR) {
        Some((b, a)) => (b, a),
        None => (fixture, ""),
    };
    let mut line = 0u32;
    let mut line_start = 0usize;
    let bytes = before.as_bytes();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'\n' || (b == b'\r' && bytes.get(i + 1) != Some(&b'\n')) {
            line += 1;
            line_start = i + 1;
        }
        i += 1;
    }
    let tail = before.get(line_start..).unwrap_or("");
    let character = match encoding {
        "utf-8" => tail.len(),
        "utf-32" => tail.chars().count(),
        _ => tail.encode_utf16().count(),
    } as u32;
    (format!("{before}{after}"), line, character)
}

/// A file URI for an absolute path, percent-encoding what a URI may not carry.
fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

struct Lsp {
    client: Connection,
    server: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    rt: Option<tokio::runtime::Runtime>,
    dir: tempfile::TempDir,
    next_id: i32,
    /// Every `publishDiagnostics` params object received, in order.
    diagnostics: Vec<Value>,
    /// The position encoding the server chose at `initialize`.
    encoding: String,
}

impl Lsp {
    /// A server over an empty workspace, not yet initialized.
    fn new() -> Lsp {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonical, so macOS `/var` vs `/private/var` never splits a path.
        let root = dir.path().canonicalize().expect("canonical root");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let engine: Arc<dyn LspEngine> = Arc::new(FakeEngine {
            root,
            registry: registry(),
        });
        let (client, server_end) = Connection::memory();
        let handle = rt.handle().clone();
        let server = std::thread::spawn(move || {
            let _guard = handle.enter();
            plugin_buildfile::pluginbuildfile::lsp_run(server_end, engine)
        });
        Lsp {
            client,
            server: Some(server),
            rt: Some(rt),
            dir,
            next_id: 0,
            diagnostics: Vec::new(),
            encoding: "utf-16".to_string(),
        }
    }

    /// A server, initialized with default client capabilities.
    fn start() -> Lsp {
        let mut lsp = Lsp::new();
        lsp.initialize();
        lsp
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonical root")
    }

    /// Write a file into the workspace (on disk, not opened).
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, text).expect("write fixture");
        path
    }

    fn uri(&self, rel: &str) -> String {
        file_uri(&self.root().join(rel))
    }

    fn send(&self, msg: Message) {
        self.client.sender.send(msg).expect("server hung up");
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(Message::Notification(Notification {
            method: method.to_string(),
            params,
        }));
    }

    /// Send a request and wait for its response, collecting notifications.
    fn request_raw(&mut self, method: &str, params: Value) -> lsp_server::Response {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.send(Message::Request(Request {
            id: id.clone(),
            method: method.to_string(),
            params,
        }));
        self.wait_response(&id)
    }

    fn wait_response(&mut self, id: &RequestId) -> lsp_server::Response {
        loop {
            let msg = self
                .client
                .receiver
                .recv_timeout(TIMEOUT)
                .expect("no response within the timeout: the server hangs");
            match msg {
                Message::Response(resp) if &resp.id == id => return resp,
                Message::Response(_) | Message::Request(_) => {}
                Message::Notification(n) => {
                    if n.method == "textDocument/publishDiagnostics" {
                        self.diagnostics.push(n.params);
                    }
                }
            }
        }
    }

    /// Send a request and return its `result`, failing on an error response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let resp = self.request_raw(method, params);
        assert!(
            resp.error.is_none(),
            "{method} answered with an error: {:?}",
            resp.error
        );
        resp.result.unwrap_or(Value::Null)
    }

    fn initialize_with(&mut self, capabilities: Value) -> Value {
        let root = self.root();
        let result = self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": file_uri(&root),
                "workspaceFolders": [{"uri": file_uri(&root), "name": "ws"}],
                "capabilities": capabilities,
            }),
        );
        self.notify("initialized", json!({}));
        if let Some(enc) = result
            .pointer("/capabilities/positionEncoding")
            .and_then(Value::as_str)
        {
            self.encoding = enc.to_string();
        }
        result
    }

    /// Send `shutdown` then `exit`, and return what the server loop returned.
    fn shutdown_and_exit(&mut self) -> anyhow::Result<()> {
        let resp = self.request_raw("shutdown", Value::Null);
        if let Some(e) = resp.error {
            anyhow::bail!("shutdown answered with an error: {e:?}");
        }
        self.notify("exit", Value::Null);
        self.join()
    }

    /// Wait (bounded) for the server loop to return.
    fn join(&mut self) -> anyhow::Result<()> {
        let Some(server) = self.server.take() else {
            anyhow::bail!("the server was already joined");
        };
        let deadline = std::time::Instant::now() + TIMEOUT;
        while !server.is_finished() {
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("the server loop did not return within {TIMEOUT:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        server
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the server thread panicked")))
    }

    /// The error code of a request's response, if it failed.
    fn error_code(&mut self, method: &str, params: Value) -> Option<i32> {
        self.request_raw(method, params).error.map(|e| e.code)
    }

    fn initialize(&mut self) -> Value {
        self.initialize_with(json!({}))
    }

    /// Open `rel` with the fixture's text (cursor marker stripped). The file
    /// is also written to disk so load resolution sees it.
    fn open(&mut self, rel: &str, fixture: &str) -> Doc {
        let (text, line, character) = split_cursor_in(fixture, &self.encoding);
        self.write(rel, &text);
        let uri = self.uri(rel);
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": "starlark", "version": 1, "text": text,
            }}),
        );
        Doc {
            uri,
            line,
            character,
            version: 1,
        }
    }

    /// Replace the whole text of an open document (full sync), returning the
    /// new cursor.
    fn change(&mut self, doc: &Doc, fixture: &str) -> Doc {
        let (text, line, character) = split_cursor_in(fixture, &self.encoding);
        let version = doc.version + 1;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": doc.uri, "version": version},
                "contentChanges": [{"text": text}],
            }),
        );
        Doc {
            uri: doc.uri.clone(),
            line,
            character,
            version,
        }
    }

    fn close(&mut self, doc: &Doc) {
        self.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": doc.uri}}),
        );
    }

    /// Round-trip one request, so every notification the server sent before
    /// handling it has been collected.
    fn barrier(&mut self) {
        let uri = self.uri("__barrier__/BUILD");
        self.request(
            "textDocument/hover",
            json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
        );
    }

    /// The diagnostics most recently published for `doc`.
    fn last_diagnostics(&self, doc: &Doc) -> Option<Vec<Value>> {
        self.diagnostics
            .iter()
            .rev()
            .find(|d| d["uri"] == json!(doc.uri))
            .map(|d| d["diagnostics"].as_array().cloned().unwrap_or_default())
    }

    fn at(&self, doc: &Doc) -> Value {
        json!({"textDocument": {"uri": doc.uri}, "position": doc.position()})
    }

    fn completion(&mut self, doc: &Doc) -> Value {
        let params = self.at(doc);
        self.request("textDocument/completion", params)
    }

    fn hover(&mut self, doc: &Doc) -> Value {
        let params = self.at(doc);
        self.request("textDocument/hover", params)
    }

    fn definition(&mut self, doc: &Doc) -> Value {
        let params = self.at(doc);
        self.request("textDocument/definition", params)
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        // Best effort: ask the server to shut down, then hang up. Neither may
        // block a failing test, so nothing here waits on the server.
        self.next_id += 1;
        let _sent = self.client.sender.send(Message::Request(Request {
            id: RequestId::from(self.next_id),
            method: "shutdown".to_string(),
            params: Value::Null,
        }));
        let _sent = self.client.sender.send(Message::Notification(Notification {
            method: "exit".to_string(),
            params: Value::Null,
        }));
        drop(self.server.take());
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

// ---------------------------------------------------------------------------
// Response readers.
// ---------------------------------------------------------------------------

/// The completion items of a response, whichever shape it came in.
fn items(result: &Value) -> Vec<Value> {
    match result {
        Value::Array(items) => items.clone(),
        Value::Object(list) => list
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn labels(result: &Value) -> Vec<String> {
    items(result)
        .iter()
        .filter_map(|i| i.get("label").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn has(result: &Value, label: &str) -> bool {
    labels(result).iter().any(|l| l == label)
}

/// The hover's text, whichever shape it came in.
fn hover_text(result: &Value) -> String {
    let Some(contents) = result.get("contents") else {
        return String::new();
    };
    let one = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    match contents {
        Value::Array(parts) => parts.iter().map(one).collect::<Vec<_>>().join("\n\n"),
        other => one(other),
    }
}

/// `(uri, line)` of every location in a definition response.
fn targets(result: &Value) -> Vec<(String, u64)> {
    let all = match result {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![result.clone()],
        _ => Vec::new(),
    };
    all.iter()
        .filter_map(|l| {
            let uri = l
                .get("targetUri")
                .or_else(|| l.get("uri"))?
                .as_str()?
                .to_string();
            let range = l.get("targetSelectionRange").or_else(|| l.get("range"))?;
            Some((uri, range.get("start")?.get("line")?.as_u64()?))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Per-site-kind tests. Phase 1 freezes today's behaviour: an answer that is
// known to be wrong is asserted as it is and marked `KNOWN WRONG`, so the
// resolver phase flips it on purpose.
// ---------------------------------------------------------------------------

#[test]
fn hover_on_target_callee_shows_schema_for_selected_driver() {
    let mut lsp = Lsp::start();
    let doc = lsp.open(
        "BUILD",
        "tar│get(name = \"x\", driver = \"exec\", cmd = \"true\")\n",
    );
    let text = hover_text(&lsp.hover(&doc));
    assert!(text.contains("cmd"), "narrowed to exec's fields: {text}");
    assert!(!text.contains("ldflags"), "not go_binary's: {text}");

    // KNOWN WRONG: a file that rebinds `target` still gets heph's schema.
    let doc = lsp.open(
        "rebound/BUILD",
        "def target(**kw):\n    pass\n\ntar│get(name = \"x\", driver = \"exec\")\n",
    );
    let text = hover_text(&lsp.hover(&doc));
    assert!(
        text.contains("cmd"),
        "today: schema shown although rebound: {text}"
    );
}

#[test]
fn completion_in_driver_value_lists_drivers_in_both_quote_styles() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"│\")\n");
    let result = lsp.completion(&doc);
    assert!(
        has(&result, "exec") && has(&result, "go_binary"),
        "{result}"
    );

    // KNOWN WRONG: single quotes get no driver names.
    let doc = lsp.open("single/BUILD", "target(name = \"x\", driver = '│')\n");
    let result = lsp.completion(&doc);
    assert!(
        !has(&result, "exec"),
        "today: single quotes unsupported: {result}"
    );

    // KNOWN WRONG: a `driver = "…"` outside any `target(` still completes.
    let doc = lsp.open("outside/BUILD", "driver = \"│\"\n");
    let result = lsp.completion(&doc);
    assert!(
        has(&result, "exec"),
        "today: completes outside target(: {result}"
    );
}

#[test]
fn completion_in_provider_value_lists_providers() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "provider_state(provider = \"│\")\n");
    let result = lsp.completion(&doc);
    // KNOWN WRONG: provider names are never offered.
    assert!(!has(&result, "go"), "today: no provider names: {result}");
}

#[test]
fn completion_in_target_call_offers_only_missing_keys() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"exec\", │)\n");
    let result = lsp.completion(&doc);
    assert!(has(&result, "cmd"), "exec's field: {result}");
    // KNOWN WRONG: keys already present are offered again.
    assert!(has(&result, "name"), "today: present key offered: {result}");

    // Value position: stock passes through (a local is offered, no keys).
    let doc = lsp.open(
        "value/BUILD",
        "MY_CMD = \"x\"\ntarget(name = \"x\", driver = \"exec\", cmd = M│)\n",
    );
    let result = lsp.completion(&doc);
    assert!(
        !has(&result, "env"),
        "no schema keys on the value side: {result}"
    );
}

#[test]
fn completion_in_unclosed_target_call_still_resolves() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"exec\", │");
    let result = lsp.completion(&doc);
    assert!(has(&result, "cmd"), "{result}");

    // Reached by an edit from a parsing document, so a stale AST exists.
    let doc = lsp.open("edited/BUILD", "target(name = \"x\", driver = \"exec\")\n");
    let doc = lsp.change(&doc, "target(name = \"x\", driver = \"exec\", │");
    let result = lsp.completion(&doc);
    assert!(has(&result, "cmd"), "{result}");
}

#[test]
fn completion_in_provider_state_offers_provider_then_schema() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "provider_state(│)\n");
    let result = lsp.completion(&doc);
    // KNOWN WRONG: the base `provider` key is not offered before a provider.
    assert!(!has(&result, "provider"), "today: no base key: {result}");

    let doc = lsp.open("chosen/BUILD", "provider_state(provider = \"go\", │)\n");
    let result = lsp.completion(&doc);
    assert!(has(&result, "go_codegen_root"), "{result}");
}

#[test]
fn completion_after_heph_dot_lists_plugins_and_core() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = heph.│\n");
    let result = lsp.completion(&doc);
    assert_eq!(labels(&result), vec!["core", "fs"], "{result}");
    for item in items(&result) {
        assert_eq!(item["kind"], json!(9), "MODULE kind: {item}");
    }
}

#[test]
fn completion_and_hover_on_plugin_function() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = heph.fs.│\n");
    let result = lsp.completion(&doc);
    assert!(has(&result, "join"), "{result}");
    let join = items(&result)
        .into_iter()
        .find(|i| i["label"] == "join")
        .expect("join item");
    // KNOWN WRONG: completion carries a plain-string doc, hover a Markdown one.
    assert!(
        join["documentation"].is_string(),
        "today: plain doc: {join}"
    );

    let doc = lsp.open("hover/BUILD", "x = heph.fs.jo│in(\"a\", \"b\")\n");
    let text = hover_text(&lsp.hover(&doc));
    assert!(text.contains("Join two path segments."), "{text}");
}

#[test]
fn completion_and_definition_on_colon_and_slash_addresses() {
    let mut lsp = Lsp::start();
    lsp.write(
        "lib/BUILD",
        "target(name = \"util\", driver = \"exec\", cmd = \"true\")\n",
    );
    let doc = lsp.open("BUILD", "target(name = \"x\", deps = [\"//lib:│\"])\n");
    let result = lsp.completion(&doc);
    assert!(has(&result, "//lib:util"), "{result}");

    // KNOWN WRONG: a relative `:name` never completes.
    let doc = lsp.open(
        "lib/BUILD",
        "target(name = \"util\", driver = \"exec\", cmd = \"true\")\ntarget(name = \"y\", deps = [\":│\"])\n",
    );
    let result = lsp.completion(&doc);
    assert!(
        !has(&result, ":util"),
        "today: no relative completion: {result}"
    );

    let doc = lsp.open(
        "def/BUILD",
        "target(name = \"x\", deps = [\"//lib:u│til\"])\n",
    );
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].0.ends_with("/lib/BUILD"), "{found:?}");
}

#[test]
fn definition_on_load_symbol_lands_on_the_def_line() {
    let mut lsp = Lsp::start();
    lsp.write(
        "lib/BUILD",
        "# helpers\n\ndef make_name(s):\n    return s\n",
    );
    let doc = lsp.open(
        "BUILD",
        "load(\"//lib\", \"make_name\")\n\ntarget(name = make_│name(\"t\"))\n",
    );
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].0.ends_with("/lib/BUILD"), "{found:?}");
    assert_eq!(found[0].1, 2, "the def line: {found:?}");
}

#[test]
fn rebound_target_gets_no_heph_schema_in_any_feature() {
    let mut lsp = Lsp::start();
    let doc = lsp.open(
        "BUILD",
        "def target(**kw):\n    pass\n\ntarget(name = \"x\", driver = \"exec\", bogus = 1, │)\n",
    );
    let result = lsp.completion(&doc);
    // KNOWN WRONG: completion still offers heph's keys on a rebound target.
    assert!(has(&result, "cmd"), "today: keys offered: {result}");
    // Diagnostics already agree: no unknown-field error for `bogus`.
    let flagged = lsp
        .diagnostics
        .iter()
        .flat_map(|d| d["diagnostics"].as_array().cloned().unwrap_or_default())
        .any(|d| d["message"].as_str().is_some_and(|m| m.contains("bogus")));
    assert!(
        !flagged,
        "diagnostics skip a rebound target: {:?}",
        lsp.diagnostics
    );
}

#[test]
fn every_heph_item_carries_a_text_edit_and_markdown_doc() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"exec\", │)\n");
    let result = lsp.completion(&doc);
    let cmd = items(&result)
        .into_iter()
        .find(|i| i["label"] == "cmd")
        .expect("cmd item");
    // KNOWN WRONG: no text edit, plain-string documentation.
    assert!(cmd.get("textEdit").is_none(), "today: no text edit: {cmd}");
    assert!(cmd["documentation"].is_string(), "today: plain doc: {cmd}");
}

#[test]
fn initialize_keeps_stock_capabilities_and_adds_formatting() {
    let mut lsp = Lsp::new();
    let result = lsp.initialize();
    let caps = &result["capabilities"];
    assert_eq!(caps["documentFormattingProvider"], json!(true), "{caps}");
    // The stock capabilities survive.
    assert_eq!(caps["hoverProvider"], json!(true), "{caps}");
    assert!(caps["definitionProvider"].is_object(), "{caps}");
    assert_eq!(caps["textDocumentSync"], json!(1), "full sync: {caps}");
    // Trigger characters are declared once, extended rather than overwritten.
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        json!([".", "\"", "'", "/", ":"]),
        "{caps}"
    );
    assert_eq!(caps["positionEncoding"], json!("utf-16"), "{caps}");
}

#[test]
fn colon_trigger_completes_only_in_address_shaped_strings() {
    let mut lsp = Lsp::start();
    lsp.write(
        "lib/BUILD",
        "target(name = \"util\", driver = \"exec\", cmd = \"true\")\n",
    );
    let doc = lsp.open("BUILD", "x = [\"//lib:│\"]\n");
    let result = lsp.completion(&doc);
    assert!(has(&result, "//lib:util"), "{result}");
}

// ---------------------------------------------------------------------------
// Vendored parity: what the stock server did, the vendored modules keep doing,
// except where the vendored crate's NOTICE lists a fix.
// ---------------------------------------------------------------------------

#[test]
fn parity_local_symbol_goto() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "NAME = \"x\"\n\ntarget(name = NA│ME)\n");
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found, vec![(doc.uri.clone(), 0)], "the assignment line");
}

#[test]
fn parity_loaded_file_goto() {
    let mut lsp = Lsp::start();
    lsp.write("lib/BUILD", "def make_name(s):\n    return s\n");
    let doc = lsp.open("BUILD", "load(\"//l│ib\", \"make_name\")\n");
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].0.ends_with("/lib/BUILD"),
        "the loaded file: {found:?}"
    );
}

#[test]
fn parity_diagnostics_published_and_cleared_on_close() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \n");
    lsp.barrier();
    let published = lsp.last_diagnostics(&doc).expect("diagnostics published");
    assert!(!published.is_empty(), "the parse error is published");
    lsp.close(&doc);
    lsp.barrier();
    let cleared = lsp
        .last_diagnostics(&doc)
        .expect("diagnostics re-published");
    assert!(cleared.is_empty(), "closing clears them: {cleared:?}");
}

#[test]
fn parity_builtin_hover() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = l│en([1])\n");
    let text = hover_text(&lsp.hover(&doc));
    assert!(text.contains("len"), "the builtin's doc: {text}");
}

#[test]
fn parity_string_completion() {
    let mut lsp = Lsp::start();
    lsp.write("lib/BUILD", "def make_name(s):\n    return s\n");
    let doc = lsp.open("BUILD", "load(\"//│\", \"make_name\")\n");
    let result = lsp.completion(&doc);
    assert!(has(&result, "//lib"), "package paths complete: {result}");
}

// ---------------------------------------------------------------------------
// The owned loop: lifecycle, document lifetime, URIs, position encoding.
// ---------------------------------------------------------------------------

/// JSON-RPC / LSP error codes the loop answers with.
const SERVER_NOT_INITIALIZED: i32 = -32002;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;

#[test]
fn lifecycle_initialize_shutdown_exit_returns() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(│)\n");
    assert!(has(&lsp.completion(&doc), "name"));
    lsp.shutdown_and_exit()
        .expect("the loop returns cleanly after shutdown and exit");
}

#[test]
fn request_before_initialize_is_rejected() {
    let mut lsp = Lsp::new();
    let uri = lsp.uri("BUILD");
    let code = lsp.error_code(
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
    );
    assert_eq!(code, Some(SERVER_NOT_INITIALIZED));
    // The handshake still works afterwards.
    lsp.initialize();
    let doc = lsp.open("BUILD", "target(│)\n");
    assert!(has(&lsp.completion(&doc), "name"));
}

#[test]
fn request_after_shutdown_is_rejected() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(│)\n");
    let resp = lsp.request_raw("shutdown", Value::Null);
    assert!(resp.error.is_none(), "{resp:?}");
    let params = lsp.at(&doc);
    assert_eq!(
        lsp.error_code("textDocument/completion", params),
        Some(INVALID_REQUEST)
    );
    lsp.notify("exit", Value::Null);
    lsp.join().expect("exit after shutdown returns cleanly");
}

#[test]
fn cancel_request_is_acknowledged() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(│)\n");
    // Every request is answered before the next message is read, so a
    // cancellation names a request that is already done: it changes nothing
    // and the loop carries on.
    lsp.notify("$/cancelRequest", json!({"id": lsp.next_id}));
    lsp.notify("$/cancelRequest", json!({"id": 9999}));
    assert!(has(&lsp.completion(&doc), "name"));
}

#[test]
fn unknown_method_is_an_error() {
    let mut lsp = Lsp::start();
    assert_eq!(
        lsp.error_code("workspace/symbol", json!({"query": "x"})),
        Some(METHOD_NOT_FOUND)
    );
    // An unknown notification is ignored, and the loop keeps serving.
    lsp.notify("heph/unknownNotification", json!({}));
    let doc = lsp.open("BUILD", "target(│)\n");
    assert!(has(&lsp.completion(&doc), "name"));
}

#[test]
fn did_close_frees_the_document_model() {
    let mut lsp = Lsp::start();
    let a = lsp.open("a/BUILD", "target(name = \"a\")\n");
    let b = lsp.open("b/BUILD", "target(name = \"b\")\n");
    let counts = |lsp: &mut Lsp| lsp.request("heph/documents", Value::Null);
    assert_eq!(
        counts(&mut lsp),
        json!({"documents": 2, "parses": 2, "indexes": 2})
    );
    lsp.close(&a);
    assert_eq!(
        counts(&mut lsp),
        json!({"documents": 1, "parses": 1, "indexes": 1})
    );
    lsp.close(&b);
    assert_eq!(
        counts(&mut lsp),
        json!({"documents": 0, "parses": 0, "indexes": 0})
    );
}

#[test]
fn request_after_did_close_is_stock() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(│)\n");
    assert!(has(&lsp.completion(&doc), "name"));
    lsp.close(&doc);
    let result = lsp.completion(&doc);
    assert!(
        !has(&result, "name"),
        "a closed document gets no heph answer: {result}"
    );
    assert!(lsp.hover(&doc).is_null() || hover_text(&lsp.hover(&doc)).is_empty());
}

#[test]
fn uri_with_spaces_and_non_ascii_round_trips() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("a dir/café/BUILD", "NAME = \"x\"\n\ntarget(name = NA│ME)\n");
    assert!(doc.uri.contains("a%20dir/caf%C3%A9/BUILD"), "{}", doc.uri);
    // The definition comes back on the same URI the client sent.
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found, vec![(doc.uri.clone(), 0)]);
    // So do the diagnostics.
    let broken = lsp.open("a dir/café/x.BUILD", "broken(\n");
    lsp.barrier();
    assert!(
        lsp.last_diagnostics(&broken).is_some_and(|d| !d.is_empty()),
        "the parse error is published on the client's URI: {:?}",
        lsp.diagnostics
    );
}

#[test]
fn position_encoding_negotiates_utf8_utf16_utf32() {
    // The column of `dri│` follows a non-ASCII name, so each encoding counts
    // it differently; the answer must be the same in all three.
    let fixture = "target(name = \"café😀\", dri│)\n";
    for (offered, chosen) in [
        (json!(["utf-8", "utf-16"]), "utf-8"),
        (json!(["utf-16"]), "utf-16"),
        (json!(["utf-32"]), "utf-32"),
        (json!(["latin-1"]), "utf-16"),
        (json!([]), "utf-16"),
    ] {
        let mut lsp = Lsp::new();
        let result = lsp.initialize_with(json!({"general": {"positionEncodings": offered}}));
        assert_eq!(
            result["capabilities"]["positionEncoding"],
            json!(chosen),
            "offered {offered}"
        );
        let doc = lsp.open("BUILD", fixture);
        let result = lsp.completion(&doc);
        assert!(has(&result, "driver"), "{chosen}: {result}");
    }
    // No `general` capabilities at all: fails closed to UTF-16.
    let mut lsp = Lsp::new();
    let result = lsp.initialize();
    assert_eq!(result["capabilities"]["positionEncoding"], json!("utf-16"));
}
