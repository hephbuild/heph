//! Guest-side SDK for heph wasm plugins.
//!
//! A plugin author implements [`Provider`] and/or [`Driver`] against the prost
//! `pb` types, and calls the engine back through the [`Host`] callback surface.
//! The thin generated-bindings glue (decode pb in, dispatch, encode pb out, map
//! errors to the WIT `plugin-error`) lives in the guest crate; this SDK supplies
//! the pb-typed traits, the [`PluginError`] type, and the encode/decode helpers
//! so that glue stays mechanical.
//!
//! Payloads are protobuf-encoded `heph.plugin.v1` messages — the same wire
//! schema the proto and shm transports use (see `crates/plugin-abi/wit`).

use prost::Message;

/// Re-export of the prost wire types so guests need not depend on plugin-abi.
pub use plugin_abi::pb;

/// Typed error category. Mirrors the WIT `error-kind` / wire `*.Kind` enums so
/// errors are classified structurally — never by matching the message string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Other,
    NotFound,
    Cycle,
    Cancelled,
    Unimplemented,
}

/// An error crossing the wasm boundary: a typed kind plus a human message.
#[derive(Debug, Clone)]
pub struct PluginError {
    pub kind: ErrorKind,
    pub message: String,
}

impl PluginError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }
    pub fn unimplemented(what: &str) -> Self {
        Self::new(ErrorKind::Unimplemented, format!("{what} not implemented"))
    }
}

/// Decode a protobuf request payload, mapping a malformed payload to a typed
/// error rather than a panic.
pub fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, PluginError> {
    M::decode(bytes).map_err(|e| PluginError::other(format!("decoding request: {e}")))
}

/// Encode a protobuf response payload.
pub fn encode<M: Message>(msg: &M) -> Vec<u8> {
    msg.encode_to_vec()
}

/// The host callback surface (AbiHost), pb-typed. The guest glue implements this
/// by calling the generated WIT `host` imports; provider/driver code receives it
/// as `&dyn Host` and uses it to resolve dependencies through the engine.
pub trait Host {
    fn result(&self, req: pb::ResultRequest) -> Result<pb::ResultResponse, PluginError>;
    fn note_dep(&self, req: pb::NoteDepRequest) -> Result<pb::NoteDepResponse, PluginError>;
    fn query(&self, req: pb::QueryRequest) -> Result<pb::QueryResponse, PluginError>;
}

/// A provider implemented in wasm. Mirrors the in-process `hplugin::Provider`
/// surface that is relevant to the remote data path.
pub trait Provider {
    /// Provider name.
    fn config(&self) -> Result<pb::ConfigResponse, PluginError>;

    /// Resolve one target. May call back into `host` to resolve dependencies.
    fn get(&self, req: pb::GetRequest, host: &dyn Host) -> Result<pb::GetResponse, PluginError>;
}

/// A driver implemented in wasm. Mirrors `hplugin::Driver`.
pub trait Driver {
    /// Driver name.
    fn config(&self) -> Result<pb::ConfigResponse, PluginError>;

    /// Parse a `TargetSpec` into a `TargetDef`.
    fn parse(&self, req: pb::ParseRequest) -> Result<pb::ParseResponse, PluginError>;

    /// Execute the target, returning output artifacts.
    fn run(&self, req: pb::RunRequest) -> Result<pb::RunResponse, PluginError>;
}

/// Convenience for guests that serve a config name.
pub fn config_response(name: &str) -> pb::ConfigResponse {
    pb::ConfigResponse {
        name: name.to_string(),
    }
}
