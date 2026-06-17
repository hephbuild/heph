//! Guest side of the native (mux-free) stable transport: wrap an author's
//! `Provider` / `ManagedDriver` as [`StableProvider`] / [`StableManagedDriver`].
//!
//! Cold requests/responses cross as prost `pb::Frame` bytes (the response `Body`
//! the mux serve loop would have sent, returned directly instead). `get` receives
//! the host executor natively ([`DynExecutor`]) so the plugin's hot callbacks are
//! direct calls — no mux, no channels, no task spawn (see ai-docs/PERFORMANCE.md).

use crate::guest::GuestExecutor;
use anyhow::Result;
use hcore::hartifactcontent::{Content, WalkEntry};
use hcore::hasync::StdCancellationToken;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunInput, ManagedRunRequest};
use hmodel::htpkg::PkgBuf;
use hplugin::driver::{
    ApplyTransitiveRequest, ConfigRequest as DriverConfigRequest, ParseRequest, RunInput,
    RunRequest, inputartifact,
};
use hplugin::provider::{
    ConfigRequest, GetError, GetRequest, ListPackagesRequest, ListRequest, ProbeRequest, Provider,
    ProviderExecutor,
};
use hplugin_stabby::abi::{DynExecutor, StableManagedDriver, StableProvider};
use plugin_abi::convert;
use plugin_abi::pb;
use plugin_abi::pb::frame::Body;
use prost::Message;
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::string::String as SString;
use stabby::vec::Vec as SVec;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

/// The cdylib's own tokio runtime. A loaded cdylib's statically-linked tokio is a
/// separate instance from the host's, so async work that touches the reactor (a
/// driver `run` shelling out via `proc_exec`) must run here, not on the host
/// worker that polls our returned future. Sized like the engine's runtime
/// (plugin-go parks workers via `block_in_place` per subprocess chunk).
fn cdylib_runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(8);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(n)
            .max_blocking_threads(8 * n + 64)
            .enable_all()
            .build()
            .expect("build cdylib plugin runtime")
    })
}

fn unary(body: Body) -> SVec<u8> {
    let f = pb::Frame {
        id: 0,
        body: Some(body),
    };
    SVec::from(f.encode_to_vec().as_slice())
}

fn stream(bodies: Vec<Body>) -> SVec<u8> {
    let mut buf = Vec::new();
    for b in bodies {
        let f = pb::Frame {
            id: 0,
            body: Some(b),
        };
        // Encoding into a growable Vec is infallible; bind the Result to satisfy
        // #[must_use] without an explicit drop (the value is Copy).
        let _encoded = f.encode_length_delimited(&mut buf);
    }
    SVec::from(buf.as_slice())
}

fn err_body(message: String) -> Body {
    Body::Error(pb::Error {
        kind: pb::error::Kind::Other as i32,
        message,
    })
}

fn is_cycle(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<hplugin::error::CycleError>(e).is_some()
}

fn get_error_kind(e: &anyhow::Error) -> pb::get_error::Kind {
    if is_cycle(e) {
        pb::get_error::Kind::Cycle
    } else if hplugin::error::is_cancelled(e) {
        pb::get_error::Kind::Cancelled
    } else {
        pb::get_error::Kind::Other
    }
}

/// Wrap a real provider as an ABI-stable [`hplugin_stabby::abi::DynProvider`] handle
/// (in-process; the cdylib entry produces the same handle across the boundary).
pub fn make_dyn_provider(provider: Arc<dyn Provider>) -> hplugin_stabby::abi::DynProvider {
    stabby::boxed::Box::new(StableProviderImpl { provider }).into()
}

/// Wrap a real managed driver as an ABI-stable [`hplugin_stabby::abi::DynManagedDriver`].
pub fn make_dyn_managed_driver(
    driver: Arc<dyn ManagedDriver>,
) -> hplugin_stabby::abi::DynManagedDriver {
    stabby::boxed::Box::new(StableManagedDriverImpl { driver }).into()
}

/// Wraps an author `Provider` as a [`StableProvider`].
pub struct StableProviderImpl {
    pub provider: Arc<dyn Provider>,
}

impl StableProvider for StableProviderImpl {
    extern "C" fn config(&self) -> SString {
        self.provider
            .config(ConfigRequest {})
            .map(|r| r.name)
            .unwrap_or_default()
            .into()
    }

    extern "C" fn list<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        stabby::boxed::Box::new(async move {
            let req = pb::ListRequest::decode(&req[..]).unwrap_or_default();
            let tok = StdCancellationToken::new();
            let lreq = ListRequest {
                request_id: req.request_id,
                package: PkgBuf::from(req.package),
                states: req.states.into_iter().map(convert::state_from_pb).collect(),
            };
            let mut bodies = Vec::new();
            match provider.list(lreq, &tok).await {
                Ok(iter) => {
                    for item in iter {
                        match item {
                            Ok(lr) => bodies.push(Body::StreamItem(pb::StreamItem {
                                item: pb::ListResponse {
                                    addr: Some(convert::addr_to_pb(&lr.addr)),
                                }
                                .encode_to_vec()
                                .into(),
                            })),
                            Err(e) => {
                                bodies.push(stream_err(e.to_string()));
                                return stream(bodies);
                            }
                        }
                    }
                    bodies.push(Body::StreamEnd(pb::StreamEnd { error: None }));
                }
                Err(e) => bodies.push(stream_err(e.to_string())),
            }
            stream(bodies)
        })
        .into()
    }

    extern "C" fn list_packages<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        stabby::boxed::Box::new(async move {
            let req = pb::ListPackagesRequest::decode(&req[..]).unwrap_or_default();
            let tok = StdCancellationToken::new();
            let lreq = ListPackagesRequest {
                prefix: PkgBuf::from(req.prefix),
            };
            let mut bodies = Vec::new();
            match provider.list_packages(lreq, &tok).await {
                Ok(iter) => {
                    for item in iter {
                        match item {
                            Ok(lpr) => bodies.push(Body::StreamItem(pb::StreamItem {
                                item: pb::ListPackageResponse {
                                    pkg: lpr.pkg.as_str().to_string(),
                                }
                                .encode_to_vec()
                                .into(),
                            })),
                            Err(e) => {
                                bodies.push(stream_err(e.to_string()));
                                return stream(bodies);
                            }
                        }
                    }
                    bodies.push(Body::StreamEnd(pb::StreamEnd { error: None }));
                }
                Err(e) => bodies.push(stream_err(e.to_string())),
            }
            stream(bodies)
        })
        .into()
    }

    extern "C" fn get<'a>(&'a self, req: SVec<u8>, exec: DynExecutor) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        stabby::boxed::Box::new(async move {
            let req = pb::GetRequest::decode(&req[..]).unwrap_or_default();
            let executor: Arc<dyn ProviderExecutor> = Arc::new(GuestExecutor::new(exec));
            let tok = StdCancellationToken::new();
            let greq = GetRequest {
                request_id: req.request_id,
                addr: convert::addr_from_pb(req.addr.unwrap_or_default()),
                states: req.states.into_iter().map(convert::state_from_pb).collect(),
                executor,
            };
            let body = match provider.get(greq, &tok).await {
                Ok(gr) => Body::GetResp(pb::GetResponse {
                    target_spec: Some(convert::target_spec_to_pb(&gr.target_spec)),
                }),
                Err(GetError::NotFound) => Body::GetErr(pb::GetError {
                    kind: pb::get_error::Kind::NotFound as i32,
                    message: String::new(),
                }),
                Err(GetError::Other(e)) => Body::GetErr(pb::GetError {
                    kind: get_error_kind(&e) as i32,
                    message: e.to_string(),
                }),
            };
            unary(body)
        })
        .into()
    }

    extern "C" fn probe<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        stabby::boxed::Box::new(async move {
            let req = pb::ProbeRequest::decode(&req[..]).unwrap_or_default();
            let tok = StdCancellationToken::new();
            let preq = ProbeRequest {
                request_id: req.request_id,
                package: PkgBuf::from(req.package),
            };
            let body = match provider.probe(preq, &tok).await {
                Ok(pr) => Body::ProbeResp(pb::ProbeResponse {
                    states: pr.states.iter().map(convert::state_to_pb).collect(),
                }),
                Err(e) => err_body(e.to_string()),
            };
            unary(body)
        })
        .into()
    }
}

fn stream_err(message: String) -> Body {
    Body::StreamEnd(pb::StreamEnd {
        error: Some(pb::Error {
            kind: pb::error::Kind::Other as i32,
            message,
        }),
    })
}

/// Wraps an author `ManagedDriver` as a [`StableManagedDriver`].
pub struct StableManagedDriverImpl {
    pub driver: Arc<dyn ManagedDriver>,
}

impl StableManagedDriver for StableManagedDriverImpl {
    extern "C" fn config(&self) -> SString {
        self.driver
            .config(DriverConfigRequest {})
            .map(|r| r.name)
            .unwrap_or_default()
            .into()
    }

    extern "C" fn parse<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let driver = Arc::clone(&self.driver);
        stabby::boxed::Box::new(async move {
            let req = pb::ParseRequest::decode(&req[..]).unwrap_or_default();
            let tok = StdCancellationToken::new();
            let preq = ParseRequest {
                request_id: req.request_id,
                target_spec: Arc::new(convert::target_spec_from_pb(
                    req.target_spec.unwrap_or_default(),
                )),
            };
            let body = match driver.parse(preq, &tok).await {
                Ok(resp) => match convert::target_def_to_pb(&resp.target_def) {
                    Ok(td) => Body::ParseResp(pb::ParseResponse {
                        target_def: Some(td),
                    }),
                    Err(e) => err_body(e.to_string()),
                },
                Err(e) => err_body(e.to_string()),
            };
            unary(body)
        })
        .into()
    }

    extern "C" fn apply_transitive<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let driver = Arc::clone(&self.driver);
        stabby::boxed::Box::new(async move {
            let req = pb::ApplyTransitiveRequest::decode(&req[..]).unwrap_or_default();
            let tok = StdCancellationToken::new();
            let target_def = match convert::target_def_from_pb(req.target_def.unwrap_or_default()) {
                Ok(td) => td,
                Err(e) => return unary(err_body(e.to_string())),
            };
            let areq = ApplyTransitiveRequest {
                request_id: req.request_id,
                target_def,
                sandbox: convert::sandbox_from_pb(req.sandbox.unwrap_or_default()),
            };
            let body = match driver.apply_transitive(areq, &tok).await {
                Ok(resp) => match convert::target_def_to_pb(&resp.target_def) {
                    Ok(td) => Body::ApplyTransitiveResp(pb::ApplyTransitiveResponse {
                        target_def: Some(td),
                    }),
                    Err(e) => err_body(e.to_string()),
                },
                Err(e) => err_body(e.to_string()),
            };
            unary(body)
        })
        .into()
    }

    extern "C" fn run<'a>(&'a self, req: SVec<u8>, shell: bool) -> DynFuture<'a, SVec<u8>> {
        let driver = Arc::clone(&self.driver);
        stabby::boxed::Box::new(async move {
            // `run` shells out via the reactor — execute on the cdylib's own
            // runtime, bridge the result back to the host's polling task.
            let (tx, rx) = tokio::sync::oneshot::channel();
            cdylib_runtime().spawn(async move {
                // Receiver dropped only if the host gave up; ignore send failure.
                drop(tx.send(run_impl(driver, req, shell).await));
            });
            rx.await
                .unwrap_or_else(|_| unary(err_body("cdylib run task dropped".into())))
        })
        .into()
    }
}

async fn run_impl(driver: Arc<dyn ManagedDriver>, req: SVec<u8>, shell: bool) -> SVec<u8> {
    let req = pb::ManagedRunRequest::decode(&req[..]).unwrap_or_default();
    let tok = StdCancellationToken::new();
    let target = match convert::target_def_from_pb(req.target.unwrap_or_default()) {
        Ok(t) => t,
        Err(e) => return unary(err_body(e.to_string())),
    };
    let request_id = req.request_id;
    let hashin = req.hashin;
    let sandbox_dir = PathBuf::from(req.sandbox_dir);
    let run_inputs: Vec<RunInput> = req.inputs.iter().map(run_input_from_pb).collect();
    let managed_inputs: Vec<ManagedRunInput> =
        req.inputs.into_iter().map(managed_input_from_pb).collect();
    let rr = RunRequest {
        request_id: &request_id,
        target: &target,
        tree_root_path: PathBuf::from(req.tree_root_path),
        inputs: run_inputs,
        hashin: hashin.as_str(),
        stdin: None,
        stdout: None,
        stderr: None,
        sandbox_dir: sandbox_dir.clone(),
    };
    let mrr = ManagedRunRequest {
        request: rr,
        sandbox_dir,
        sandbox_ws_dir: PathBuf::from(req.sandbox_ws_dir),
        sandbox_pkg_dir: PathBuf::from(req.sandbox_pkg_dir),
        inputs: managed_inputs,
    };
    let result = if shell {
        driver.run_shell(mrr, &tok).await
    } else {
        driver.run(mrr, &tok).await
    };
    let body = match result {
        Ok(resp) => Body::ManagedRunResp(pb::ManagedRunResponse {
            artifacts: resp
                .artifacts
                .iter()
                .map(convert::output_artifact_to_pb)
                .collect(),
        }),
        Err(e) => err_body(e.to_string()),
    };
    unary(body)
}

fn run_input_from_pb(mi: &pb::ManagedRunInput) -> RunInput {
    let ty = match pb::InputArtifactType::try_from(mi.r#type).unwrap_or(pb::InputArtifactType::Dep)
    {
        pb::InputArtifactType::Support => inputartifact::Type::Support,
        _ => inputartifact::Type::Dep,
    };
    RunInput {
        artifact: inputartifact::InputArtifact {
            r#type: ty,
            origin_id: mi.origin_id.clone(),
            // Input bytes live on the shared filesystem (unpack_root); read from
            // disk, never from this Content.
            content: Arc::new(NullContent),
        },
        origin_id: mi.origin_id.clone(),
        source_addr: convert::addr_from_pb(mi.source_addr.clone().unwrap_or_default()),
        filters: mi.filters.clone(),
        annotations: mi
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

fn managed_input_from_pb(mi: pb::ManagedRunInput) -> ManagedRunInput {
    let input = run_input_from_pb(&mi);
    ManagedRunInput {
        input,
        list_path: mi.list_path.map(PathBuf::from),
        unpack_root: PathBuf::from(mi.unpack_root),
    }
}

/// Placeholder Content for materialized run inputs — bytes live on the shared
/// filesystem (unpack_root), so this is never read.
struct NullContent;
impl Content for NullContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        anyhow::bail!("managed run input content is on disk (unpack_root), not streamed")
    }
    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        anyhow::bail!("managed run input content is on disk (unpack_root), not streamed")
    }
    fn hashout(&self) -> Result<String> {
        Ok(String::new())
    }
}
