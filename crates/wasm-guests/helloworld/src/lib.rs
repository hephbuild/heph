//! Hello-world heph plugin as a wasm component: a provider and a driver.
//!
//! - provider `get` resolves any requested addr to a `hello`-driver target, and
//!   calls back into the host (`query`) to prove the bidirectional callback path
//!   — the count is stitched into a label.
//! - driver `parse` produces a `TargetDef` carrying the greeting in its opaque
//!   `raw_def`; `run` returns one inline output artifact ("hello world").
//!
//! The author code is written against the pb-typed SDK traits; the glue below
//! (decode in / dispatch / encode out / map errors to the WIT `plugin-error`)
//! is the only thing that touches the generated bindings.

#[allow(warnings)]
mod bindings;

use bindings::exports::heph::plugin::api::Guest;
use bindings::heph::plugin::host;
use bindings::heph::plugin::types::{ErrorKind as WErrorKind, PluginError as WErr};

use heph_plugin_sdk_wasm as sdk;
use sdk::pb;
use sdk::{Driver, ErrorKind, Host, PluginError, Provider};

// ---- error mapping between the SDK's typed error and the WIT record ----

fn kind_to_wit(k: ErrorKind) -> WErrorKind {
    match k {
        ErrorKind::Other => WErrorKind::Other,
        ErrorKind::NotFound => WErrorKind::NotFound,
        ErrorKind::Cycle => WErrorKind::Cycle,
        ErrorKind::Cancelled => WErrorKind::Cancelled,
        ErrorKind::Unimplemented => WErrorKind::Unimplemented,
    }
}

fn kind_from_wit(k: WErrorKind) -> ErrorKind {
    match k {
        WErrorKind::Other => ErrorKind::Other,
        WErrorKind::NotFound => ErrorKind::NotFound,
        WErrorKind::Cycle => ErrorKind::Cycle,
        WErrorKind::Cancelled => ErrorKind::Cancelled,
        WErrorKind::Unimplemented => ErrorKind::Unimplemented,
    }
}

fn to_wit(e: PluginError) -> WErr {
    WErr {
        kind: kind_to_wit(e.kind),
        message: e.message,
    }
}

fn from_wit(e: WErr) -> PluginError {
    PluginError {
        kind: kind_from_wit(e.kind),
        message: e.message,
    }
}

// ---- host bridge: the SDK callback surface over the generated WIT imports ----

struct HostBridge;

impl Host for HostBridge {
    fn result(&self, req: pb::ResultRequest) -> Result<pb::ResultResponse, PluginError> {
        let bytes = host::resolve(&sdk::encode(&req)).map_err(from_wit)?;
        sdk::decode(&bytes)
    }
    fn note_dep(&self, req: pb::NoteDepRequest) -> Result<pb::NoteDepResponse, PluginError> {
        let bytes = host::note_dep(&sdk::encode(&req)).map_err(from_wit)?;
        sdk::decode(&bytes)
    }
    fn query(&self, req: pb::QueryRequest) -> Result<pb::QueryResponse, PluginError> {
        let bytes = host::query(&sdk::encode(&req)).map_err(from_wit)?;
        sdk::decode(&bytes)
    }
}

// ---- the hello-world provider + driver (pb-typed author code) ----

struct HelloProvider;

impl Provider for HelloProvider {
    fn config(&self) -> Result<pb::ConfigResponse, PluginError> {
        Ok(sdk::config_response("hello"))
    }

    fn get(&self, req: pb::GetRequest, host: &dyn Host) -> Result<pb::GetResponse, PluginError> {
        let addr = req.addr.clone().unwrap_or_default();
        // Prove the guest->host callback path: query the engine for everything in
        // this package and fold the count into a label so the host can observe it.
        let q = host.query(pb::QueryRequest {
            request_id: req.request_id.clone(),
            matcher: Some(pb::Matcher {
                kind: Some(pb::matcher::Kind::Package(addr.package.clone())),
            }),
            extra_skip: vec![],
        })?;
        let mut config = std::collections::HashMap::new();
        config.insert(
            "greeting".to_string(),
            pb::Value {
                kind: Some(pb::value::Kind::StringVal("hello world".to_string())),
            },
        );
        Ok(pb::GetResponse {
            target_spec: Some(pb::TargetSpec {
                addr: Some(addr),
                driver: "hello".to_string(),
                config,
                labels: vec![format!("queried:{}", q.addrs.len())],
                transitive: None,
            }),
        })
    }
}

struct HelloDriver;

impl Driver for HelloDriver {
    fn config(&self) -> Result<pb::ConfigResponse, PluginError> {
        Ok(sdk::config_response("hello"))
    }

    fn parse(&self, req: pb::ParseRequest) -> Result<pb::ParseResponse, PluginError> {
        let spec = req.target_spec.unwrap_or_default();
        let raw_def = pb::RawDefBlob {
            driver: "hello".to_string(),
            format: pb::raw_def_blob::Format::Json as i32,
            data: br#"{"greeting":"hello world"}"#.to_vec().into(),
        };
        Ok(pb::ParseResponse {
            target_def: Some(pb::TargetDef {
                addr: spec.addr,
                labels: spec.labels,
                raw_def: Some(raw_def),
                inputs: vec![],
                outputs: vec![pb::Output {
                    group: String::new(),
                    paths: vec![],
                }],
                support_files: vec![],
                cache: None,
                pty: false,
                hash: vec![].into(),
                transparent: false,
            }),
        })
    }

    fn run(&self, _req: pb::RunRequest) -> Result<pb::RunResponse, PluginError> {
        // Outputs are fresh bytes the driver "produced"; cross inline as Raw.
        let artifact = pb::OutputArtifactRef {
            group: String::new(),
            name: "out".to_string(),
            r#type: pb::ArtifactType::Output as i32,
            content: Some(pb::output_artifact_ref::Content::Raw(pb::ContentRaw {
                data: b"hello world".to_vec().into(),
                path: "out.txt".to_string(),
                x: false,
            })),
            hashout: String::new(),
        };
        Ok(pb::RunResponse {
            artifacts: vec![artifact],
        })
    }
}

// ---- generated-export glue ----

struct Component;

impl Guest for Component {
    fn provider_config() -> Result<Vec<u8>, WErr> {
        HelloProvider
            .config()
            .map(|r| sdk::encode(&r))
            .map_err(to_wit)
    }

    fn provider_get(req: Vec<u8>) -> Result<Vec<u8>, WErr> {
        let req: pb::GetRequest = sdk::decode(&req).map_err(to_wit)?;
        HelloProvider
            .get(req, &HostBridge)
            .map(|r| sdk::encode(&r))
            .map_err(to_wit)
    }

    fn driver_config() -> Result<Vec<u8>, WErr> {
        HelloDriver
            .config()
            .map(|r| sdk::encode(&r))
            .map_err(to_wit)
    }

    fn driver_parse(req: Vec<u8>) -> Result<Vec<u8>, WErr> {
        let req: pb::ParseRequest = sdk::decode(&req).map_err(to_wit)?;
        HelloDriver
            .parse(req)
            .map(|r| sdk::encode(&r))
            .map_err(to_wit)
    }

    fn driver_run(req: Vec<u8>) -> Result<Vec<u8>, WErr> {
        let req: pb::RunRequest = sdk::decode(&req).map_err(to_wit)?;
        HelloDriver
            .run(req)
            .map(|r| sdk::encode(&r))
            .map_err(to_wit)
    }
}

bindings::export!(Component with_types_in bindings);
