//! Editor-shaped end-to-end tests for the BUILD-file language server.
//!
//! Each test starts the real server over an in-memory connection, sends
//! `initialize` and `didOpen` exactly as an editor would, then a request at a
//! position, and asserts on the JSON the editor receives. Fixtures mark the
//! cursor with `│`, so no column is ever counted by hand; every receive is a
//! bounded `recv_timeout`, so a hang fails the test instead of stalling CI.
//!
//! The registry is a [`FakeEngine`]: two drivers with distinct fields, one
//! provider with a state schema, and one plugin function. Resolutions (the
//! site, the reason an answer is empty, what a present answer could not do)
//! are read through the test-support `heph/explain` request, the same
//! classification `heph tool build-lsp --explain` prints.
#![expect(
    clippy::string_slice,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    reason = "a test harness: a bad index or an unexpected case is a failed test"
)]

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
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

include!("fixtures/lsp_fixtures.rs");

/// How long any single receive may take before the test fails as a hang.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The cursor marker in fixtures.
const CURSOR: char = '│';

// ---------------------------------------------------------------------------
// The registry.
// ---------------------------------------------------------------------------

/// Holds the registry snapshot back until opened, so a test can observe the
/// server while providers are still loading.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn release(&self) {
        *self.open.lock().expect("gate lock") = true;
        self.cv.notify_all();
    }
}

struct FakeEngine {
    root: PathBuf,
    registry: Arc<FunctionRegistry>,
    gate: Option<Arc<Gate>>,
}

impl FakeEngine {
    fn wait_for_gate(&self) {
        if let Some(gate) = &self.gate {
            let mut open = gate.open.lock().expect("gate lock");
            while !*open {
                open = gate.cv.wait(open).expect("gate wait");
            }
        }
    }
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
        self.wait_for_gate();
        vec!["exec".to_string(), "go_binary".to_string()]
    }
    fn provider_names(&self) -> Vec<String> {
        vec!["go".to_string()]
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
    text: String,
    line: u32,
    character: u32,
    version: i32,
}

impl Doc {
    fn position(&self) -> Value {
        json!({"line": self.line, "character": self.character})
    }
}

/// Strip the `│` marker and return the text plus its 0-based (line, column),
/// the column counted in the named position encoding. Lines end at `\n`,
/// `\r\n` or a lone `\r`, as the LSP defines them. Without a marker the
/// cursor is at the end of the text.
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

/// The byte offset of a UTF-16 LSP position in `text`.
fn byte_of(text: &str, pos: &Value) -> usize {
    let line = pos["line"].as_u64().expect("line") as usize;
    let character = pos["character"].as_u64().expect("character") as usize;
    let mut starts = vec![0usize];
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'\n' {
            starts.push(i + 1);
        } else if b == b'\r' {
            if bytes.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            starts.push(i + 1);
        }
        i += 1;
    }
    let start = starts[line];
    let mut units = 0;
    for (off, c) in text[start..].char_indices() {
        if units >= character {
            return start + off;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// `text` with a completion item's text edit applied.
fn apply(text: &str, item: &Value) -> String {
    let edit = &item["textEdit"];
    let start = byte_of(text, &edit["range"]["start"]);
    let end = byte_of(text, &edit["range"]["end"]);
    format!(
        "{}{}{}",
        &text[..start],
        edit["newText"].as_str().expect("newText"),
        &text[end..]
    )
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
    gate: Option<Arc<Gate>>,
    next_id: i32,
    /// Every `publishDiagnostics` params object received, in order.
    diagnostics: Vec<Value>,
    /// Every `window/showMessage` params object received, in order.
    messages: Vec<Value>,
    /// The position encoding the server chose at `initialize`.
    encoding: String,
}

impl Lsp {
    /// A server over an empty workspace, not yet initialized.
    fn new() -> Lsp {
        Lsp::with(
            None,
            plugin_buildfile::pluginbuildfile::LspRunOptions::default(),
        )
    }

    /// A server whose registry snapshot waits on the returned gate, and whose
    /// `initialize` does not wait for it.
    fn new_gated() -> (Lsp, Arc<Gate>) {
        let gate = Arc::new(Gate::default());
        let lsp = Lsp::with(
            Some(Arc::clone(&gate)),
            plugin_buildfile::pluginbuildfile::LspRunOptions {
                registry_wait: Duration::ZERO,
            },
        );
        (lsp, gate)
    }

    fn with(
        gate: Option<Arc<Gate>>,
        options: plugin_buildfile::pluginbuildfile::LspRunOptions,
    ) -> Lsp {
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
            gate: gate.clone(),
        });
        let (client, server_end) = Connection::memory();
        let handle = rt.handle().clone();
        let server = std::thread::spawn(move || {
            let _guard = handle.enter();
            plugin_buildfile::pluginbuildfile::lsp_run_with(server_end, engine, options)
        });
        Lsp {
            client,
            server: Some(server),
            rt: Some(rt),
            dir,
            gate,
            next_id: 0,
            diagnostics: Vec::new(),
            messages: Vec::new(),
            encoding: "utf-16".to_string(),
        }
    }

    /// A server, initialized with default client capabilities.
    fn start() -> Lsp {
        let mut lsp = Lsp::new();
        lsp.initialize();
        lsp
    }

    /// [`Lsp::start`] over the standard workspace: `lib/BUILD` declares
    /// target `util` on line 1, `x/y.star` defines `foo` and `fun`.
    fn workspace() -> Lsp {
        let lsp = Lsp::start();
        lsp.write(
            "lib/BUILD",
            "# lib\ntarget(name = \"util\", driver = \"exec\", cmd = \"true\")\n",
        );
        lsp.write("x/y.star", "def foo():\n    pass\n\ndef fun():\n    pass\n");
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
                Message::Notification(n) => match n.method.as_str() {
                    "textDocument/publishDiagnostics" => self.diagnostics.push(n.params),
                    "window/showMessage" => self.messages.push(n.params),
                    _ => {}
                },
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

    fn initialize(&mut self) -> Value {
        self.initialize_with(json!({}))
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
            text,
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
            text,
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

    /// Completion as an editor sends it when `trigger` was just typed.
    fn completion_triggered(&mut self, doc: &Doc, trigger: &str) -> Value {
        let mut params = self.at(doc);
        params["context"] = json!({"triggerKind": 2, "triggerCharacter": trigger});
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

    /// The resolver's classification of the cursor (test support).
    fn explain(&mut self, doc: &Doc) -> Value {
        let params = self.at(doc);
        self.request("heph/explain", params)
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.release();
        }
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

fn item(result: &Value, label: &str) -> Value {
    items(result)
        .into_iter()
        .find(|i| i["label"] == label)
        .unwrap_or_else(|| panic!("no `{label}` item in {result}"))
}

/// Whether an item follows heph's conventions (sortText `0_<label>`).
fn is_heph(item: &Value) -> bool {
    item["sortText"].as_str() == item["label"].as_str().map(|l| format!("0_{l}")).as_deref()
}

fn heph_items(result: &Value) -> Vec<Value> {
    items(result).into_iter().filter(is_heph).collect()
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

/// The four fields of a resolution's site every broken variant must keep:
/// kind, enclosing call, keyword and string prefix.
fn fields(explained: &Value) -> (Value, Value, Value, Value) {
    let site = &explained["site"];
    (
        site["kind"].clone(),
        site["call"].clone(),
        site["keyword"].clone(),
        site["prefix"].clone(),
    )
}

fn kind(explained: &Value) -> String {
    explained["site"]["kind"].as_str().unwrap_or("").to_string()
}

fn reason(explained: &Value) -> Option<String> {
    explained["reason"]["reason"].as_str().map(str::to_string)
}

fn degraded(explained: &Value) -> Vec<String> {
    explained["degraded"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["degraded"].as_str().map(str::to_string))
        .collect()
}

fn degraded_entry(explained: &Value, name: &str) -> Value {
    explained["degraded"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["degraded"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no `{name}` in {explained}"))
}

/// The base `target(` keys.
const TARGET_KEYS: [&str; 5] = ["approval", "driver", "labels", "name", "transitive"];

// ---------------------------------------------------------------------------
// Per site kind.
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

    let doc = lsp.open(
        "go/BUILD",
        "tar│get(name = \"x\", driver = \"go_binary\")\n",
    );
    let text = hover_text(&lsp.hover(&doc));
    assert!(text.contains("ldflags") && !text.contains("cmd"), "{text}");

    // A file that rebinds `target` gets no heph schema.
    let doc = lsp.open(
        "rebound/BUILD",
        "def target(**kw):\n    pass\n\ntar│get(name = \"x\", driver = \"exec\")\n",
    );
    let text = hover_text(&lsp.hover(&doc));
    assert!(!text.contains("cmd"), "rebound: no schema: {text}");
}

/// The stock hover renders an undocumented global as its name in a level-one
/// heading, and answers anywhere on a dotted chain with its root's hover. A
/// hover that is only a heading says nothing, so none is sent.
#[test]
fn hover_that_is_only_a_heading_is_dropped() {
    let mut lsp = Lsp::start();
    for (path, src) in [
        (
            "root/BUILD",
            "x = he│ph.fs.join(\"a\", \"b\").removesuffix(\".proto\")\n",
        ),
        (
            "method/BUILD",
            "x = heph.fs.join(\"a\", \"b\").removesu│ffix(\".proto\")\n",
        ),
    ] {
        let doc = lsp.open(path, src);
        let result = lsp.hover(&doc);
        assert!(result.is_null(), "{path}: {}", hover_text(&result));
    }

    // A documented local `def` still gets the stock hover, heading and all.
    let doc = lsp.open(
        "def/BUILD",
        "def f():\n    \"\"\"Makes things.\"\"\"\n    pass\n\nf│()\n",
    );
    let text = hover_text(&lsp.hover(&doc));
    assert!(text.contains("Makes things."), "{text}");
}

#[test]
fn completion_in_driver_value_lists_drivers_in_both_quote_styles() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"│\")\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["exec", "go_binary"]);

    let doc = lsp.open("single/BUILD", "target(name = 'x', driver = '│')\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["exec", "go_binary"]);

    // Replace policy: no stock items beside them.
    let doc = lsp.open("prefix/BUILD", "target(name = \"x\", driver = \"ex│\")\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["exec"]);

    // Nothing outside a `target(` call.
    let doc = lsp.open("outside/BUILD", "driver = \"│\"\n");
    let result = lsp.completion(&doc);
    assert!(heph_items(&result).is_empty(), "{result}");
}

#[test]
fn completion_in_provider_value_lists_providers() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "provider_state(provider = \"│\")\n");
    let result = lsp.completion(&doc);
    assert_eq!(labels(&result), vec!["go"]);
    assert_eq!(
        item(&result, "go")["kind"],
        json!(20),
        "ENUM_MEMBER, like drivers"
    );
    let doc = lsp.open("single/BUILD", "provider_state(provider = '│')\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["go"]);
}

#[test]
fn completion_in_target_call_offers_only_missing_keys() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"exec\", │)\n");
    let result = lsp.completion(&doc);
    assert_eq!(
        labels(&result),
        vec!["approval", "cmd", "env", "labels", "transitive"],
        "only the missing heph keys"
    );

    // A non-driver value position: stock locals pass through, no keys.
    let doc = lsp.open(
        "value/BUILD",
        "MY_CMD = \"x\"\ntarget(name = \"x\", driver = \"exec\", cmd = M│)\n",
    );
    let result = lsp.completion(&doc);
    assert!(has(&result, "MY_CMD"), "the local passes through: {result}");
    assert!(heph_items(&result).is_empty(), "no schema keys: {result}");
}

#[test]
fn completion_in_unclosed_target_call_still_resolves() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"exec\", │");
    let result = lsp.completion(&doc);
    assert!(has(&result, "cmd") && !has(&result, "name"), "{result}");

    // Reached by an edit from a parsing document, so a stale AST exists.
    let doc = lsp.open("edited/BUILD", "target(name = \"x\", driver = \"exec\")\n");
    let doc = lsp.change(&doc, "target(name = \"x\", driver = \"exec\", │");
    let result = lsp.completion(&doc);
    assert!(has(&result, "cmd") && !has(&result, "name"), "{result}");
}

#[test]
fn completion_in_provider_state_offers_provider_then_schema() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "provider_state(│)\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["provider"]);

    let doc = lsp.open("chosen/BUILD", "provider_state(provider = \"go\", │)\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["go_codegen_root"]);
}

#[test]
fn completion_after_heph_dot_lists_plugins_and_core() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = heph.│\n");
    let result = lsp.completion(&doc);
    assert_eq!(labels(&result), vec!["core", "fs"], "sorted, nothing else");
    for item in items(&result) {
        assert_eq!(item["kind"], json!(9), "MODULE kind: {item}");
    }
}

#[test]
fn completion_and_hover_on_plugin_function() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = heph.fs.│\n");
    let result = lsp.completion(&doc);
    assert_eq!(labels(&result), vec!["join"]);
    let join = item(&result, "join");
    assert_eq!(join["documentation"]["kind"], json!("markdown"), "{join}");

    let doc = lsp.open("hover/BUILD", "x = heph.fs.jo│in(\"a\", \"b\")\n");
    let hover = lsp.hover(&doc);
    assert_eq!(hover["contents"]["kind"], json!("markdown"));
    // One doc format for both features.
    assert_eq!(
        hover["contents"]["value"], join["documentation"]["value"],
        "hover and completion render the same doc"
    );
    assert!(hover_text(&hover).contains("Join two path segments."));
}

#[test]
fn completion_and_definition_on_colon_and_slash_addresses() {
    let mut lsp = Lsp::workspace();
    let doc = lsp.open("BUILD", "target(name = \"x\", deps = [\"//lib:│\"])\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["//lib:util"]);
    let doc = lsp.open("pkgs/BUILD", "target(name = \"x\", deps = [\"//l│\"])\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["//lib"]);

    // A relative `:name` completes as well as resolves.
    let doc = lsp.open(
        "lib/BUILD",
        "# lib\ntarget(name = \"util\", driver = \"exec\", cmd = \"true\")\ntarget(name = \"y\", deps = [\":│\"])\n",
    );
    let result = lsp.completion(&doc);
    assert!(has(&result, ":util") && has(&result, ":y"), "{result}");

    let doc = lsp.open(
        "def/BUILD",
        "target(name = \"x\", deps = [\"//lib:u│til\"])\n",
    );
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found, vec![(lsp.uri("lib/BUILD"), 1)], "the target's line");

    let base = lsp.open("lib/BUILD", "x = 1\n");
    let doc = lsp.change(
        &base,
        "# lib\ntarget(name = \"util\", driver = \"exec\", cmd = \"true\")\ntarget(name = \"y\", deps = [\":ut│il\"])\n",
    );
    let found = targets(&lsp.definition(&doc));
    assert_eq!(
        found,
        vec![(lsp.uri("lib/BUILD"), 1)],
        "relative resolves too"
    );
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
    assert_eq!(found, vec![(lsp.uri("lib/BUILD"), 2)], "the def line");

    // And from the symbol inside the load statement.
    let doc = lsp.open("in_load/BUILD", "load(\"//lib\", \"make_n│ame\")\n");
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found, vec![(lsp.uri("lib/BUILD"), 2)], "the def line");
}

#[test]
fn rebound_target_gets_no_heph_schema_in_any_feature() {
    let mut lsp = Lsp::start();
    let src =
        "def target(**kw):\n    pass\n\ntarget(name = \"x\", driver = \"exec\", bogus = 1, │)\n";
    let doc = lsp.open("BUILD", src);
    let result = lsp.completion(&doc);
    assert!(heph_items(&result).is_empty(), "completion: {result}");
    let explained = lsp.explain(&doc);
    assert_eq!(reason(&explained).as_deref(), Some("callee_rebound"));

    let hover_doc = lsp.open(
        "hover/BUILD",
        &src.replace("│", "")
            .replacen("target(name", "tar│get(name", 1),
    );
    let text = hover_text(&lsp.hover(&hover_doc));
    assert!(!text.contains("cmd"), "hover: {text}");

    lsp.barrier();
    let flagged = lsp
        .last_diagnostics(&doc)
        .unwrap_or_default()
        .iter()
        .any(|d| d["message"].as_str().is_some_and(|m| m.contains("bogus")));
    assert!(
        !flagged,
        "diagnostics skip a rebound target: {:?}",
        lsp.diagnostics
    );
}

#[test]
fn every_heph_item_carries_a_text_edit_and_markdown_doc() {
    let mut lsp = Lsp::workspace();
    let cases = [
        "target(name = \"x\", driver = \"exec\", │)\n",
        "provider_state(provider = \"go\", │)\n",
        "target(name = \"x\", driver = \"│\")\n",
        "target(name = \"x\", driver = │)\n",
        "provider_state(provider = \"│\")\n",
        "x = heph.│\n",
        "x = heph.fs.│\n",
        "target(name = \"x\", deps = [\"//lib:│\"])\n",
        "target(name = \"x\", deps = [\"//│\"])\n",
        "load(\"//│\", \"x\")\n",
        "load(\"//x:y.star\", \"│\")\n",
    ];
    for (i, case) in cases.iter().enumerate() {
        let doc = lsp.open(&format!("c{i}/BUILD"), case);
        let result = lsp.completion(&doc);
        let all = items(&result);
        assert!(!all.is_empty(), "{case:?}: {result}");
        let kinds: std::collections::HashSet<String> =
            all.iter().map(|i| i["kind"].to_string()).collect();
        assert_eq!(kinds.len(), 1, "one kind per site: {case:?}: {result}");
        for item in &all {
            assert!(is_heph(item), "sortText 0_<name>: {case:?}: {item}");
            assert!(item["textEdit"]["newText"].is_string(), "{case:?}: {item}");
            assert!(item["filterText"].is_string(), "{case:?}: {item}");
            assert_eq!(
                item["documentation"]["kind"],
                json!("markdown"),
                "{case:?}: {item}"
            );
        }
    }
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
    assert!(
        caps.get("documentSymbolProvider").is_none(),
        "off until implemented"
    );
}

#[test]
fn colon_trigger_completes_only_in_address_shaped_strings() {
    let mut lsp = Lsp::workspace();
    let doc = lsp.open("BUILD", "x = [\"//lib:│\"]\n");
    assert_eq!(
        labels(&lsp.completion_triggered(&doc, ":")),
        vec!["//lib:util"]
    );

    let doc = lsp.open("dict/BUILD", "LOCAL = 1\nx = {\"a\":│}\n");
    assert_eq!(
        lsp.completion_triggered(&doc, ":"),
        json!([]),
        "dict-literal colon"
    );

    let doc = lsp.open("prose/BUILD", "x = \"a:│b\"\n");
    assert_eq!(lsp.completion_triggered(&doc, ":"), json!([]), "prose");

    // A `/` outside a string likewise.
    let doc = lsp.open("slash/BUILD", "LOCAL = 1\nx = 4 /│\n");
    assert_eq!(lsp.completion_triggered(&doc, "/"), json!([]), "division");
}

#[test]
fn site_request_before_did_open_is_stock() {
    let mut lsp = Lsp::start();
    let uri = lsp.uri("BUILD");
    let result = lsp.request(
        "textDocument/completion",
        json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
    );
    assert_eq!(result, json!([]));
    let doc = lsp.open("BUILD", "│");
    assert_eq!(kind(&lsp.explain(&doc)), "none", "empty document");
}

// ---------------------------------------------------------------------------
// The site kind × feature matrix: every cell asserted, or asserted null.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Feature {
    Items,
    Hover,
    Definition,
}

fn matrix(site: &str, feature: Feature) {
    let mut lsp = Lsp::workspace();
    let fixture = match site {
        "callee" => "tar│get(name = \"x\", driver = \"exec\")\n",
        "driver_value" => "target(name = \"x\", driver = \"ex│ec\")\n",
        "provider_value" => "provider_state(provider = \"g│o\")\n",
        "target_key" => "target(name = \"x\", driver = \"exec\", c│md = \"true\")\n",
        "state_key" => "provider_state(provider = \"go\", go_codegen_r│oot = True)\n",
        "plugin_namespace" => "x = heph.f│s\n",
        "plugin_member" => "x = heph.fs.jo│in(\"a\", \"b\")\n",
        "address" => "target(name = \"x\", deps = [\"//lib:ut│il\"])\n",
        "load_path" => "load(\"//l│ib\", \"make_name\")\n",
        "load_symbol" => "load(\"//x:y.star\", \"fo│o\")\n",
        "none" => "MY_CMD = \"x\"\ntarget(name = \"x\", cmd = MY_C│MD)\n",
        other => panic!("unknown site {other}"),
    };
    let doc = lsp.open("m/BUILD", fixture);
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), site, "{explained}");
    match feature {
        Feature::Items => {
            let result = lsp.completion(&doc);
            assert!(result.is_array(), "always an array: {result}");
            let expected: &[&str] = match site {
                "callee" | "none" => {
                    assert!(heph_items(&result).is_empty(), "stock passes: {result}");
                    if site == "none" {
                        assert!(has(&result, "MY_CMD"), "{result}");
                    }
                    return;
                }
                "driver_value" => &["exec"],
                "provider_value" => &["go"],
                "target_key" => &["cmd"],
                "state_key" => &["go_codegen_root"],
                "plugin_namespace" => &["fs"],
                "plugin_member" => &["join"],
                "address" => &["//lib:util"],
                "load_path" => &["//lib"],
                "load_symbol" => &["foo"],
                _ => unreachable!(),
            };
            assert_eq!(labels(&result), expected, "{result}");
        }
        Feature::Hover => {
            let hover = lsp.hover(&doc);
            let needle = match site {
                "callee" => Some("cmd"),
                "driver_value" => Some("**`exec`** driver"),
                "provider_value" => Some("go_codegen_root"),
                "target_key" => Some("Command line to run."),
                "state_key" => Some("Mark this package as a Go codegen root."),
                "plugin_member" => Some("Join two path segments."),
                "plugin_namespace" | "address" | "load_path" | "load_symbol" => None,
                "none" => {
                    // Stock: an undocumented assignment has no hover, and no
                    // heph schema leaks in.
                    assert!(!hover_text(&hover).contains("Command line"), "{hover}");
                    return;
                }
                _ => unreachable!(),
            };
            match needle {
                Some(n) => assert!(hover_text(&hover).contains(n), "{site}: {hover}"),
                None => assert!(hover.is_null(), "{site}: heph has no hover: {hover}"),
            }
        }
        Feature::Definition => {
            let found = targets(&lsp.definition(&doc));
            let expected = match site {
                "address" => vec![(lsp.uri("lib/BUILD"), 1)],
                "load_path" => vec![(lsp.uri("lib/BUILD"), 0)],
                "load_symbol" => vec![(lsp.uri("x/y.star"), 0)],
                "none" => vec![(doc.uri.clone(), 0)],
                _ => Vec::new(),
            };
            assert_eq!(found, expected, "{site}");
        }
    }
}

#[test]
fn matrix_callee_items() {
    matrix("callee", Feature::Items);
}
#[test]
fn matrix_callee_hover() {
    matrix("callee", Feature::Hover);
}
#[test]
fn matrix_callee_definition() {
    matrix("callee", Feature::Definition);
}
#[test]
fn matrix_driver_value_items() {
    matrix("driver_value", Feature::Items);
}
#[test]
fn matrix_driver_value_hover() {
    matrix("driver_value", Feature::Hover);
}
#[test]
fn matrix_driver_value_definition() {
    matrix("driver_value", Feature::Definition);
}
#[test]
fn matrix_provider_value_items() {
    matrix("provider_value", Feature::Items);
}
#[test]
fn matrix_provider_value_hover() {
    matrix("provider_value", Feature::Hover);
}
#[test]
fn matrix_provider_value_definition() {
    matrix("provider_value", Feature::Definition);
}
#[test]
fn matrix_target_key_items() {
    matrix("target_key", Feature::Items);
}
#[test]
fn matrix_target_key_hover() {
    matrix("target_key", Feature::Hover);
}
#[test]
fn matrix_target_key_definition() {
    matrix("target_key", Feature::Definition);
}
#[test]
fn matrix_state_key_items() {
    matrix("state_key", Feature::Items);
}
#[test]
fn matrix_state_key_hover() {
    matrix("state_key", Feature::Hover);
}
#[test]
fn matrix_state_key_definition() {
    matrix("state_key", Feature::Definition);
}
#[test]
fn matrix_plugin_namespace_items() {
    matrix("plugin_namespace", Feature::Items);
}
#[test]
fn matrix_plugin_namespace_hover() {
    matrix("plugin_namespace", Feature::Hover);
}
#[test]
fn matrix_plugin_namespace_definition() {
    matrix("plugin_namespace", Feature::Definition);
}
#[test]
fn matrix_plugin_member_items() {
    matrix("plugin_member", Feature::Items);
}
#[test]
fn matrix_plugin_member_hover() {
    matrix("plugin_member", Feature::Hover);
}
#[test]
fn matrix_plugin_member_definition() {
    matrix("plugin_member", Feature::Definition);
}
#[test]
fn matrix_address_items() {
    matrix("address", Feature::Items);
}
#[test]
fn matrix_address_hover() {
    matrix("address", Feature::Hover);
}
#[test]
fn matrix_address_definition() {
    matrix("address", Feature::Definition);
}
#[test]
fn matrix_load_path_items() {
    matrix("load_path", Feature::Items);
}
#[test]
fn matrix_load_path_hover() {
    matrix("load_path", Feature::Hover);
}
#[test]
fn matrix_load_path_definition() {
    matrix("load_path", Feature::Definition);
}
#[test]
fn matrix_load_symbol_items() {
    matrix("load_symbol", Feature::Items);
}
#[test]
fn matrix_load_symbol_hover() {
    matrix("load_symbol", Feature::Hover);
}
#[test]
fn matrix_load_symbol_definition() {
    matrix("load_symbol", Feature::Definition);
}
#[test]
fn matrix_none_items() {
    matrix("none", Feature::Items);
}
#[test]
fn matrix_none_hover() {
    matrix("none", Feature::Hover);
}
#[test]
fn matrix_none_definition() {
    matrix("none", Feature::Definition);
}

// ---------------------------------------------------------------------------
// The merge policy.
// ---------------------------------------------------------------------------

/// The handler table has no append site: wherever heph knows the answer it
/// replaces the stock one, and elsewhere the stock one passes untouched.
#[test]
fn merge_replace_sites_drop_stock_and_append_sites_keep_it() {
    let mut lsp = Lsp::workspace();
    let prelude = "MY_LOCAL = 1\n";
    for (i, replace) in [
        "target(│)\n",
        "target(name = \"x\", driver = \"│\")\n",
        "x = heph.│\n",
        "x = [\"//lib:│\"]\n",
    ]
    .iter()
    .enumerate()
    {
        let doc = lsp.open(&format!("r{i}/BUILD"), &format!("{prelude}{replace}"));
        let result = lsp.completion(&doc);
        assert!(!items(&result).is_empty(), "{replace:?}");
        assert!(
            items(&result).iter().all(is_heph),
            "replace: only heph items at {replace:?}: {result}"
        );
        assert!(!has(&result, "MY_LOCAL"), "{replace:?}");
    }
    for (i, pass) in ["x = MY_│\n", "target(name = MY_│)\n"].iter().enumerate() {
        let doc = lsp.open(&format!("p{i}/BUILD"), &format!("{prelude}{pass}"));
        let result = lsp.completion(&doc);
        assert!(
            has(&result, "MY_LOCAL"),
            "pass through at {pass:?}: {result}"
        );
        assert!(heph_items(&result).is_empty(), "{pass:?}: {result}");
    }
}

#[test]
fn merge_filters_by_prefix_and_dedupes_by_label() {
    let mut lsp = Lsp::workspace();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"go_b│\")\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["go_binary"]);
    // A key present twice in the call is still excluded once, and no label is
    // ever doubled.
    let doc = lsp.open(
        "dup/BUILD",
        "target(name = \"a\", name = \"b\", driver = \"exec\", │)\n",
    );
    let got = labels(&lsp.completion(&doc));
    let mut unique = got.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(got.len(), unique.len(), "{got:?}");
    assert!(!got.contains(&"name".to_string()));
}

#[test]
fn text_edit_covers_token_with_and_without_closing_quote() {
    let mut lsp = Lsp::start();
    let closed = lsp.open("a/BUILD", "target(name = \"x\", driver = \"ex│\")\n");
    let open = lsp.open("b/BUILD", "target(name = \"x\", driver = \"ex│\n");
    let closed_item = item(&lsp.completion(&closed), "exec");
    let open_item = item(&lsp.completion(&open), "exec");
    // Applied, both leave the same line: the auto-paired quote is not doubled
    // and the missing one is supplied.
    assert_eq!(
        apply(&closed.text, &closed_item),
        "target(name = \"x\", driver = \"exec\")\n"
    );
    assert_eq!(
        apply(&open.text, &open_item),
        "target(name = \"x\", driver = \"exec\"\n"
    );
    // The same edit start: the opening quote's side of the value.
    let start = |d: &Doc, i: &Value| byte_of(&d.text, &i["textEdit"]["range"]["start"]);
    assert_eq!(start(&open, &open_item) + 1, start(&closed, &closed_item));
}

#[test]
fn completion_is_always_an_array_marked_complete() {
    let mut lsp = Lsp::workspace();
    for (i, fixture) in [
        "target(│)\n",
        "x = MY│\n",
        "tar│get()\n",
        "x = \"oops│\n",
        "\"\"\"\n│",
        "load(\"//│\")\n",
    ]
    .iter()
    .enumerate()
    {
        let doc = lsp.open(&format!("a{i}/BUILD"), fixture);
        let result = lsp.completion(&doc);
        assert!(result.is_array(), "{fixture:?}: {result}");
    }
    // Past the end of the document, and on an unknown document.
    let doc = lsp.open("end/BUILD", "x = 1\n");
    let past = json!({"textDocument": {"uri": doc.uri}, "position": {"line": 9, "character": 0}});
    assert_eq!(lsp.request("textDocument/completion", past), json!([]));
}

#[test]
fn empty_results_carry_a_reason() {
    let mut lsp = Lsp::start();
    // callee_rebound
    let doc = lsp.open("a/BUILD", "target = None\ntarget(│)\n");
    assert_eq!(
        reason(&lsp.explain(&doc)).as_deref(),
        Some("callee_rebound")
    );
    // in_lex_error, carrying the error's kind, span and line
    let doc = lsp.open("b/BUILD", "x = 1\ny = \"a\\x│ZZ\"\n");
    let explained = lsp.explain(&doc);
    assert_eq!(reason(&explained).as_deref(), Some("in_lex_error"));
    assert_eq!(explained["reason"]["line"], json!(1));
    assert_eq!(explained["reason"]["kind"], json!("other"));
    // not_a_site
    let doc = lsp.open("c/BUILD", "x = 1│\n");
    assert_eq!(reason(&lsp.explain(&doc)).as_deref(), Some("not_a_site"));

    // degraded: call_extent_guessed, with the line it stopped at
    let doc = lsp.open("d/BUILD", "target(dri│\nx = 1\n");
    assert_eq!(
        degraded_entry(&lsp.explain(&doc), "call_extent_guessed")["line"],
        json!(1)
    );
    // edit_to_word_end
    let doc = lsp.open("e/BUILD", "target(driver = \"ex│\n");
    assert!(degraded(&lsp.explain(&doc)).contains(&"edit_to_word_end".to_string()));
    // ast_absent: never parsed
    let doc = lsp.open("f/BUILD", "target(│");
    assert!(degraded(&lsp.explain(&doc)).contains(&"ast_absent".to_string()));
    // ast_stale, with the versions behind
    let doc = lsp.open("g/BUILD", "target()\n");
    let doc = lsp.change(&doc, "target(x\n");
    let doc = lsp.change(&doc, "target(│");
    assert_eq!(
        degraded_entry(&lsp.explain(&doc), "ast_stale")["behind"],
        json!(2)
    );
    // lex_error_before, with its span
    let doc = lsp.open("h/BUILD", "x = \"oops\ntarget(dri│\n");
    assert_eq!(
        degraded_entry(&lsp.explain(&doc), "lex_error_before")["span"],
        json!([4, 9])
    );

    // providers_loading: stock results until the registry snapshot lands.
    let (mut lsp, gate) = Lsp::new_gated();
    lsp.initialize();
    let doc = lsp.open("BUILD", "target(│)\n");
    assert_eq!(
        reason(&lsp.explain(&doc)).as_deref(),
        Some("providers_loading")
    );
    assert!(heph_items(&lsp.completion(&doc)).is_empty());
    gate.release();
    let deadline = std::time::Instant::now() + TIMEOUT;
    while reason(&lsp.explain(&doc)).is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the registry never landed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(has(&lsp.completion(&doc), "driver"));
}

#[test]
fn lexer_error_is_a_token_and_lexing_continues() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"exec\")\n");
    // Mid-edit: the string is unterminated, the call unclosed. No fallback to
    // the last good text: the current tokens answer.
    let doc = lsp.change(&doc, "target(name = \"x\", driver = \"ex│\n    dri\n");
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "driver_value", "{explained}");
    assert_eq!(explained["site"]["call"], json!("target"));
    assert_eq!(labels(&lsp.completion(&doc)), vec!["exec"]);
    // The next line still lexes: a key site in the same call, where `driver`
    // already counts as present.
    let doc = lsp.change(&doc, "target(name = \"x\", driver = \"ex\n    lab│\n");
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "target_key", "{explained}");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["labels"]);
}

// ---------------------------------------------------------------------------
// Broken documents (invariant 7): the typing states, one by one.
// ---------------------------------------------------------------------------

#[test]
fn broken_unclosed_call_at_eof_offers_keys() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "MY_LOCAL = 1\ntarget(│");
    assert_eq!(
        labels(&lsp.completion(&doc)),
        TARGET_KEYS,
        "every key, no locals"
    );
}

#[test]
fn broken_unclosed_call_filters_by_prefix() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", dri│");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["driver"]);
    let doc = lsp.open("empty/BUILD", "target(name = \"x\", │");
    let got = labels(&lsp.completion(&doc));
    assert!(!got.contains(&"name".to_string()), "name excluded: {got:?}");
}

#[test]
fn broken_missing_comma_still_resolves() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(\n    name = \"x\"\n    dri│\n)\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["driver"]);
}

#[test]
fn broken_unterminated_string_is_a_driver_site() {
    let mut lsp = Lsp::start();
    let doc = lsp.open(
        "BUILD",
        "target(name = \"x\", driver = \"ex│\n    cmd = 1,\n",
    );
    let result = lsp.completion(&doc);
    assert_eq!(labels(&result), vec!["exec"]);
    let exec = item(&result, "exec");
    let range = &exec["textEdit"]["range"];
    let (s, e) = (
        byte_of(&doc.text, &range["start"]),
        byte_of(&doc.text, &range["end"]),
    );
    assert_eq!(
        &doc.text[s..e],
        "\"ex",
        "covers the quote and the typed run"
    );
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "driver_value", "not an identifier site");
    assert!(degraded(&explained).contains(&"edit_to_word_end".to_string()));
}

#[test]
fn broken_swallowed_paren_keeps_the_call_open() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"ex│)");
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "driver_value");
    assert_eq!(explained["site"]["call"], json!("target"));
    let exec = item(&lsp.completion(&doc), "exec");
    let end = byte_of(&doc.text, &exec["textEdit"]["range"]["end"]);
    assert!(
        doc.text[end..].starts_with(')'),
        "the `)` is outside the edit"
    );
}

#[test]
fn broken_unterminated_string_crlf() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"ex│\r\n");
    let exec = item(&lsp.completion(&doc), "exec");
    let end = byte_of(&doc.text, &exec["textEdit"]["range"]["end"]);
    assert_eq!(&doc.text[end..], "\r\n", "the edit stops before the `\\r`");
    let explained = lsp.explain(&doc);
    let token = &explained["site"]["token"];
    let token_end = token[1].as_u64().expect("token end") as usize;
    assert_eq!(&doc.text[token_end..], "\n", "the span includes the `\\r`");
}

#[test]
fn broken_lone_quote_at_eof_offers_every_driver() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"│");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["exec", "go_binary"]);
    assert_eq!(lsp.explain(&doc)["site"]["prefix"], json!(""));
}

#[test]
fn broken_prefixed_unterminated_string() {
    let mut lsp = Lsp::start();
    for (i, prefix) in ["r", "b"].iter().enumerate() {
        let doc = lsp.open(
            &format!("p{i}/BUILD"),
            &format!("target(name = \"x\", driver = {prefix}\"ex│\n"),
        );
        let result = lsp.completion(&doc);
        assert_eq!(labels(&result), vec!["exec"], "{prefix}");
        let exec = item(&result, "exec");
        let s = byte_of(&doc.text, &exec["textEdit"]["range"]["start"]);
        assert_eq!(
            &doc.text[s..s + 1],
            *prefix,
            "the span opens at the prefix letter"
        );
    }
}

/// The fixture is `driver = "don't│` rather than the spec's `name = …`: a
/// `name` value is not a site, so its prefix would not be observable.
#[test]
fn broken_apostrophe_inside_unterminated_string() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(driver = \"don't│\n");
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "driver_value", "{explained}");
    assert_eq!(
        explained["site"]["prefix"],
        json!("don't"),
        "the nested ' is merged"
    );
}

#[test]
fn broken_quote_typed_before_a_string_flips_parity() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", driver = \"│\"exec\"\n");
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "driver_value");
    assert_eq!(explained["site"]["prefix"], json!(""));
    assert_eq!(labels(&lsp.completion(&doc)), vec!["exec", "go_binary"]);
    lsp.barrier();
    assert!(lsp.last_diagnostics(&doc).is_some_and(|d| !d.is_empty()));
}

#[test]
fn broken_invalid_escape_costs_the_rest_of_its_line() {
    let mut lsp = Lsp::start();
    let src = "target(driver = \"ex\\xZZ\", na│)\n    lab\n";
    let doc = lsp.open("BUILD", src);
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "none");
    assert_eq!(reason(&explained).as_deref(), Some("in_lex_error"));
    let span = &explained["reason"]["span"];
    let (s, e) = (
        span[0].as_u64().unwrap() as usize,
        span[1].as_u64().unwrap() as usize,
    );
    assert!(
        doc.text[s..e].starts_with("\\x"),
        "the root cause, the escape: {explained}"
    );
    // The published diagnostic points at the escape too, not the re-opened
    // string.
    lsp.barrier();
    let diags = lsp.last_diagnostics(&doc).unwrap_or_default();
    let escape_col = doc.text.find("\\x").unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d["range"]["start"]["character"] == json!(escape_col)),
        "{diags:?}"
    );
    let reopened_col = doc.text.find("\", na").unwrap();
    assert!(
        !diags
            .iter()
            .any(|d| d["range"]["start"]["character"] == json!(reopened_col)),
        "{diags:?}"
    );
    // The swallowed `)` leaves the extent to the extent rule; the call and its
    // earlier argument survive and count as present.
    let below = lsp.change(&doc, &src.replace("│", "").replace("    lab", "    lab│"));
    let explained = lsp.explain(&below);
    assert_eq!(kind(&explained), "target_key", "{explained}");
    assert!(
        explained["site"]["present_keys"]
            .as_array()
            .unwrap()
            .contains(&json!("driver"))
    );
    assert!(degraded(&explained).contains(&"call_extent_guessed".to_string()));
    assert_eq!(labels(&lsp.completion(&below)), vec!["labels"]);
}

#[test]
fn broken_unterminated_fstring_terminates_and_is_confined_to_its_line() {
    let mut lsp = Lsp::start();
    // Under the harness's receive timeout: a lexer that never ends would fail
    // here rather than stall.
    let doc = lsp.open("BUILD", "x = f\"ex\ntarget(name = \"x\", dri│)\n");
    let clean = lsp.open("clean/BUILD", "x = 1\ntarget(name = \"x\", dri│)\n");
    assert_eq!(lsp.completion(&doc), lsp.completion(&clean));
    let doc = lsp.open("in/BUILD", "target(driver = f\"ex│\n    name = \"x\",\n");
    lsp.completion(&doc);
}

#[test]
fn broken_unclosed_fstring_expression_does_not_swallow_later_lines() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = f\"abc{y\ntarget(name = \"x\", dri│)\n");
    let clean = lsp.open("clean/BUILD", "x = 1\ntarget(name = \"x\", dri│)\n");
    assert_eq!(lsp.completion(&doc), lsp.completion(&clean));
}

#[test]
fn broken_lex_error_on_an_earlier_line_is_confined() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = \"oops\ntarget(dri│\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["driver"]);
    assert_eq!(
        degraded_entry(&lsp.explain(&doc), "lex_error_before")["span"],
        json!([4, 9])
    );
}

#[test]
fn broken_stray_char_inside_the_call() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = $, │\n");
    let got = labels(&lsp.completion(&doc));
    assert!(got.contains(&"driver".to_string()), "{got:?}");
    assert!(
        !got.contains(&"name".to_string()),
        "name counted as present: {got:?}"
    );
    let doc = lsp.open("p/BUILD", "target(name = $, dri│\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["driver"]);
}

#[test]
fn broken_indentation_error_before_the_cursor() {
    let mut lsp = Lsp::start();
    let doc = lsp.open(
        "BUILD",
        "if x:\n        a = 1\n    b = 2\n\ty = 3\ntarget(name = \"x\", dri│)\n",
    );
    let result = lsp.completion(&doc);
    assert_eq!(labels(&result), vec!["driver"]);
    // Columns unchanged: the edit covers exactly `dri`.
    let item = item(&result, "driver");
    let range = &item["textEdit"]["range"];
    let (s, e) = (
        byte_of(&doc.text, &range["start"]),
        byte_of(&doc.text, &range["end"]),
    );
    assert_eq!(&doc.text[s..e], "dri");
}

#[test]
fn broken_tab_indented_body_still_resolves() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "def f():\n\ttarget(name = \"x\", dri│\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["driver"]);
    let doc = lsp.open("def/BUILD", "def f():│\n\ttarget(name = \"x\", dri\n");
    let result = lsp.completion(&doc);
    assert!(heph_items(&result).is_empty(), "stock: {result}");
    assert_eq!(kind(&lsp.explain(&doc)), "none");
}

#[test]
fn broken_open_triple_quote_swallows_the_rest() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = 1\n\"\"\"abc\ntarget(dri│\n");
    let explained = lsp.explain(&doc);
    assert_eq!(kind(&explained), "none");
    assert_eq!(reason(&explained).as_deref(), Some("in_lex_error"));
    assert_eq!(explained["reason"]["line"], json!(1), "the opener's line");
    assert!(
        heph_items(&lsp.completion(&doc)).is_empty(),
        "stock passes through"
    );
    lsp.barrier();
    let diags = lsp.last_diagnostics(&doc).unwrap_or_default();
    assert!(
        diags
            .iter()
            .any(|d| d["range"]["start"] == json!({"line": 1, "character": 0})),
        "the diagnostic starts at the opener: {diags:?}"
    );
}

#[test]
fn broken_cursor_above_an_open_triple_quote_answers_in_full() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\", dri│)\n\"\"\"abc\n");
    let clean = lsp.open("clean/BUILD", "target(name = \"x\", dri│)\n");
    assert_eq!(lsp.completion(&doc), lsp.completion(&clean));
}

#[test]
fn broken_mismatched_closer_before_the_cursor() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "foo(]\ntarget(dri│\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["driver"]);
}

#[test]
fn broken_mismatched_closer_after_the_cursor_ends_the_key_scan() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "deps = [ target(│ ], name = \"y\")\n");
    let got = labels(&lsp.completion(&doc));
    assert!(
        got.contains(&"name".to_string()),
        "the scan stopped at the ]: {got:?}"
    );
}

#[test]
fn broken_later_statement_does_not_matter() {
    let mut lsp = Lsp::start();
    let doc = lsp.open(
        "BUILD",
        "target(dri│)\ntarget(name = \"y\", driver = \"ex\n",
    );
    let clean = lsp.open("clean/BUILD", "target(dri│)\n");
    assert_eq!(
        lsp.completion(&doc),
        lsp.completion(&clean),
        "byte-identical"
    );
}

#[test]
fn broken_unclosed_call_ends_at_the_next_column_zero_line() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(│\nname = \"y\"\n");
    assert!(has(&lsp.completion(&doc), "name"));
    assert!(degraded(&lsp.explain(&doc)).contains(&"call_extent_guessed".to_string()));
}

#[test]
fn broken_unclosed_call_inside_a_def_ends_at_its_own_indent() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "def m():\n    target(│\n    other(name = \"y\")\n");
    assert!(has(&lsp.completion(&doc), "name"));
}

#[test]
fn broken_unclosed_call_extent_ignores_column_zero_inside_a_triple_quoted_string() {
    let mut lsp = Lsp::start();
    let doc = lsp.open(
        "BUILD",
        "target(\n    │\n    labels = \"\"\"\nfoo\n\"\"\",\n",
    );
    let got = labels(&lsp.completion(&doc));
    assert!(
        !got.contains(&"labels".to_string()),
        "labels is present: {got:?}"
    );
    assert!(got.contains(&"driver".to_string()), "{got:?}");
}

#[test]
fn broken_unclosed_call_extent_ignores_a_column_zero_comment() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(\n    │\n# note\n    labels = [\"a\"],\n");
    let got = labels(&lsp.completion(&doc));
    assert!(
        !got.contains(&"labels".to_string()),
        "labels is present: {got:?}"
    );
}

#[test]
fn broken_earlier_unclosed_call_does_not_swallow_a_later_call() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "MY_LOCAL = 1\ntarget(\nx = │\n");
    let result = lsp.completion(&doc);
    assert!(heph_items(&result).is_empty(), "stock, not keys: {result}");
    assert_eq!(kind(&lsp.explain(&doc)), "none");
}

#[test]
fn broken_hover_on_callee_in_unclosed_call() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "tar│get(name = \"x\", driver = \"exec\"\n");
    let text = hover_text(&lsp.hover(&doc));
    assert!(text.contains("cmd") && !text.contains("ldflags"), "{text}");
}

#[test]
fn broken_definition_on_address_without_a_parse() {
    let mut lsp = Lsp::workspace();
    let doc = lsp.open("BUILD", "x = (\ntarget(deps = [\"//lib:u│til\"]\n");
    let found = targets(&lsp.definition(&doc));
    assert_eq!(found, vec![(lsp.uri("lib/BUILD"), 1)], "the target's line");
}

#[test]
fn broken_unclosed_load_offers_symbols() {
    let mut lsp = Lsp::workspace();
    let doc = lsp.open("BUILD", "load(\"//x:y.star\", \"f│");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["foo", "fun"]);
}

#[test]
fn broken_plugin_namespace_with_broken_tail() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = heph.│\ny = (\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["core", "fs"]);
}

#[test]
fn completion_in_driver_value_without_a_string_inserts_quotes() {
    let mut lsp = Lsp::start();
    for (i, (fixture, after)) in [
        (
            "target(name = \"x\", driver = │)\n",
            "target(name = \"x\", driver = \"exec\")\n",
        ),
        (
            "target(name = \"x\", driver = ex│)\n",
            "target(name = \"x\", driver = \"exec\")\n",
        ),
        (
            "target(name = \"x\", driver = \"│\")\n",
            "target(name = \"x\", driver = \"exec\")\n",
        ),
    ]
    .iter()
    .enumerate()
    {
        let doc = lsp.open(&format!("q{i}/BUILD"), fixture);
        let result = lsp.completion(&doc);
        assert!(has(&result, "exec"), "{fixture:?}: {result}");
        assert_eq!(
            apply(&doc.text, &item(&result, "exec")),
            *after,
            "{fixture:?}"
        );
    }
    let doc = lsp.open("q/BUILD", "provider_state(provider = │)\n");
    let result = lsp.completion(&doc);
    assert_eq!(
        apply(&doc.text, &item(&result, "go")),
        "provider_state(provider = \"go\")\n"
    );
}

#[test]
fn broken_shadowing_survives_edits_above_the_call() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "def target(**kw):\n    pass\n\ntarget(│)\n");
    assert!(heph_items(&lsp.completion(&doc)).is_empty());
    let doc = lsp.change(&doc, "x = (\ndef target(**kw):\n    pass\n\ntarget(│)\n");
    assert!(
        heph_items(&lsp.completion(&doc)).is_empty(),
        "still no heph schema"
    );
    let explained = lsp.explain(&doc);
    assert_eq!(reason(&explained).as_deref(), Some("callee_rebound"));
    assert_eq!(degraded_entry(&explained, "ast_stale")["behind"], json!(1));
}

#[test]
fn broken_document_publishes_diagnostics_and_still_completes() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "target(name = \"x\")\n");
    let doc = lsp.change(&doc, "target(name = \"x\", dri│");
    let result = lsp.completion(&doc);
    assert_eq!(
        labels(&result),
        vec!["driver"],
        "the same version answers in full"
    );
    let published = lsp
        .diagnostics
        .iter()
        .rev()
        .find(|d| d["uri"] == json!(doc.uri) && d["version"] == json!(doc.version))
        .expect("diagnostics for the broken version");
    assert!(
        !published["diagnostics"].as_array().unwrap().is_empty(),
        "{published}"
    );
}

#[test]
fn completion_after_three_rapid_didchanges_uses_the_latest_text() {
    let mut lsp = Lsp::start();
    let doc = lsp.open("BUILD", "x = 1\n");
    let doc = lsp.change(&doc, "target(│");
    let doc = lsp.change(&doc, "provider_state(│");
    let doc = lsp.change(&doc, "target(name = \"x\", dri│)\n");
    assert_eq!(labels(&lsp.completion(&doc)), vec!["driver"]);
}

#[test]
fn broken_unclosed_call_in_def_ends_at_own_indent() {
    let mut lsp = Lsp::start();
    let doc = lsp.open(
        "BUILD",
        "def m():\n    target(│\n    target(name = \"y\", driver = \"exec\")\n",
    );
    let got = labels(&lsp.completion(&doc));
    assert!(got.contains(&"driver".to_string()), "{got:?}");
    assert!(
        got.contains(&"name".to_string()),
        "the second call's keys: {got:?}"
    );
    assert!(!got.contains(&"cmd".to_string()), "nor its driver: {got:?}");
}

// ---------------------------------------------------------------------------
// The sweeps over every fixture.
// ---------------------------------------------------------------------------

/// The four broken variants of a fixture (marker kept).
fn variants(fixture: &str, skip_c: bool) -> Vec<(&'static str, String)> {
    let at = fixture.find(CURSOR).expect("cursor marker");
    let before = &fixture[..at + CURSOR.len_utf8()];
    let mut out = vec![
        ("a", before.to_string()),
        ("b", format!("{before}\nz = \"oops\n)))\n")),
    ];
    if !skip_c {
        // A line holding an unterminated string and an astral character,
        // above the statement enclosing the cursor.
        let mut start = fixture[..at].rfind('\n').map_or(0, |i| i + 1);
        while start > 0
            && fixture[start..]
                .chars()
                .next()
                .is_some_and(|c| c == ' ' || c == '\t' || c == '\n' || c == ')')
        {
            start = fixture[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        }
        out.push((
            "c",
            format!("{}z = \"oops 😀\n{}", &fixture[..start], &fixture[start..]),
        ));
    }
    let mut crlf = String::new();
    let mut prev = '\0';
    for c in fixture.chars() {
        if c == '\n' && prev != '\r' {
            crlf.push('\r');
        }
        crlf.push(c);
        prev = c;
    }
    out.push(("d", crlf));
    out
}

#[test]
fn site_is_a_function_of_the_prefix() {
    let mut lsp = Lsp::workspace();
    for (n, (name, fixture, skip_c)) in FIXTURES.iter().enumerate() {
        // Hover-only sites read the `(` after the cursor by definition.
        if name.starts_with("callee") {
            continue;
        }
        // Labels are heph's: where the stock answer passes through it depends
        // on the last good parse, which a broken variant does not have.
        let heph_labels = |result: &Value| -> Vec<String> {
            heph_items(result)
                .iter()
                .filter_map(|i| i["label"].as_str().map(str::to_string))
                .collect()
        };
        let doc = lsp.open(&format!("o{n}/BUILD"), fixture);
        let original = lsp.explain(&doc);
        let original_labels = heph_labels(&lsp.completion(&doc));
        let in_call = !original["site"]["call"].is_null();
        let key_site = matches!(kind(&original).as_str(), "target_key" | "state_key");
        for (v, text) in variants(fixture, *skip_c) {
            // Opened fresh, and reached by didChange from the original (only
            // the second has a stale AST to exercise).
            let fresh = lsp.open(&format!("v{n}{v}/BUILD"), &text);
            let edited_base = lsp.open(&format!("e{n}{v}/BUILD"), fixture);
            let edited = lsp.change(&edited_base, &text);
            for (how, d) in [("fresh", &fresh), ("didChange", &edited)] {
                let explained = lsp.explain(d);
                assert_eq!(
                    fields(&explained),
                    fields(&original),
                    "{name} ({v}, {how}): {text:?}\n{explained}\n{original}"
                );
                let got = heph_labels(&lsp.completion(d));
                let check_equal = matches!(v, "c" | "d") || (v == "b" && !in_call);
                if check_equal {
                    assert_eq!(got, original_labels, "{name} ({v}, {how}) labels");
                } else if v == "a" && key_site {
                    for l in &original_labels {
                        assert!(got.contains(l), "{name} (a, {how}): {l} lost: {got:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn text_edit_never_removes_bytes_outside_the_token() {
    let mut lsp = Lsp::workspace();
    for (n, (name, fixture, _)) in FIXTURES.iter().enumerate() {
        let doc = lsp.open(&format!("t{n}/BUILD"), fixture);
        let explained = lsp.explain(&doc);
        let Some(token) = explained["site"]["token"].as_array() else {
            continue;
        };
        let (ts, te) = (
            token[0].as_u64().unwrap() as usize,
            token[1].as_u64().unwrap() as usize,
        );
        for item in heph_items(&lsp.completion(&doc)) {
            let range = &item["textEdit"]["range"];
            let (s, e) = (
                byte_of(&doc.text, &range["start"]),
                byte_of(&doc.text, &range["end"]),
            );
            assert!(
                ts <= s && e <= te,
                "{name}: edit {s}..{e} outside the token {ts}..{te}: {item}"
            );
            // And applying it changes no byte outside the token.
            let applied = apply(&doc.text, &item);
            assert!(applied.starts_with(&doc.text[..ts]), "{name}: {item}");
            assert!(applied.ends_with(&doc.text[te..]), "{name}: {item}");
        }
    }
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
    assert!(lsp.hover(&doc).is_null());
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
        assert_eq!(labels(&result), vec!["driver"], "{chosen}: {result}");
        // The edit comes back in the same encoding: it starts where `dri`
        // does.
        let start = &item(&result, "driver")["textEdit"]["range"]["start"];
        assert_eq!(
            start["character"].as_u64(),
            Some(u64::from(doc.character) - 3),
            "{chosen}"
        );
    }
    // No `general` capabilities at all: fails closed to UTF-16.
    let mut lsp = Lsp::new();
    let result = lsp.initialize();
    assert_eq!(result["capabilities"]["positionEncoding"], json!("utf-16"));
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
// One workspace: the folder the editor sends, inside the bound repo.
// ---------------------------------------------------------------------------

const INVALID_PARAMS: i32 = -32602;

#[test]
fn initialize_warns_on_more_than_one_workspace_folder() {
    let mut lsp = Lsp::new();
    let other = tempfile::tempdir().expect("tempdir");
    let root = lsp.root();
    lsp.request(
        "initialize",
        json!({
            "processId": null,
            "workspaceFolders": [
                {"uri": file_uri(&root), "name": "first"},
                {"uri": file_uri(other.path()), "name": "second"},
            ],
            "capabilities": {},
        }),
    );
    lsp.barrier();
    assert_eq!(lsp.messages.len(), 1, "{:?}", lsp.messages);
    assert_eq!(lsp.messages[0]["type"], json!(2), "a warning");
    // Only the first is used, and it serves.
    let doc = lsp.open("BUILD", "target(│)\n");
    assert!(has(&lsp.completion(&doc), "name"));
}

#[test]
fn initialize_refuses_a_folder_outside_the_repo() {
    let mut lsp = Lsp::new();
    let other = tempfile::tempdir().expect("tempdir");
    let code = lsp.error_code(
        "initialize",
        json!({
            "processId": null,
            "workspaceFolders": [{"uri": file_uri(other.path()), "name": "elsewhere"}],
            "capabilities": {},
        }),
    );
    assert_eq!(code, Some(INVALID_PARAMS));
}

#[test]
fn document_outside_the_root_gets_stock_with_a_reason() {
    let mut lsp = Lsp::start();
    let other = tempfile::tempdir().expect("tempdir");
    let path = other
        .path()
        .canonicalize()
        .expect("canonical")
        .join("BUILD");
    std::fs::write(&path, "target()\n").expect("write");
    let uri = file_uri(&path);
    lsp.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri, "languageId": "starlark", "version": 1, "text": "target()\n",
        }}),
    );
    let doc = Doc {
        uri,
        text: "target()\n".to_string(),
        line: 0,
        character: 7,
        version: 1,
    };
    assert_eq!(reason(&lsp.explain(&doc)).as_deref(), Some("outside_root"));
    assert!(heph_items(&lsp.completion(&doc)).is_empty());
}
