//! Guest side of the native (mux-free) stable transport: wrap an author's
//! `Provider` / `ManagedDriver` as [`StableProvider`] / [`StableManagedDriver`].
//!
//! Cold requests/responses cross as prost `pb::Frame` bytes (the response `Body`
//! the mux serve loop would have sent, returned directly instead). `get` receives
//! the host executor natively ([`DynExecutor`]) so the plugin's hot callbacks are
//! direct calls — no mux, no channels, no task spawn (see ai-docs/PERFORMANCE.md).

use crate::guest::GuestExecutor;
use anyhow::{Context, Result};
use hcore::hartifactcontent::tar::TarPacker;
use hcore::hartifactcontent::{Content, WalkEntry, WalkEntryKind};
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
            // The host already materialized this input onto the shared filesystem
            // before invoking the driver. Back the content by those on-disk files
            // so a driver may read it (`walk`/`reader`) just like in-process —
            // no bytes are re-shipped over the boundary.
            content: Arc::new(DiskInputContent {
                unpack_root: PathBuf::from(&mi.unpack_root),
                list_path: mi.list_path.clone().map(PathBuf::from),
            }),
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

/// [`Content`] for a managed run input, backed by the files the host already
/// materialized onto the shared filesystem under `unpack_root`. `list_path` (Dep
/// inputs) names the exact absolute paths of this input's files; without it
/// (Support inputs) the whole `unpack_root` tree is walked. `walk` reads those
/// files from disk; `reader` re-tars them (artifacts are tar by convention).
struct DiskInputContent {
    unpack_root: PathBuf,
    list_path: Option<PathBuf>,
}

impl DiskInputContent {
    /// Absolute paths of this input's materialized files.
    fn files(&self) -> Result<Vec<PathBuf>> {
        if let Some(lp) = &self.list_path {
            let data = std::fs::read_to_string(lp)
                .with_context(|| format!("read input list file {}", lp.display()))?;
            return Ok(data
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect());
        }
        // Support inputs carry no list file; walk the materialized tree.
        let mut out = Vec::new();
        let mut stack = vec![self.unpack_root.clone()];
        while let Some(dir) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("read dir {}", dir.display()));
                }
            };
            for entry in rd {
                let entry = entry?;
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    stack.push(entry.path());
                } else {
                    out.push(entry.path());
                }
            }
        }
        Ok(out)
    }
}

impl Content for DiskInputContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        let mut packer = TarPacker::new();
        for abs in self.files()? {
            let rel = abs.strip_prefix(&self.unpack_root).unwrap_or(&abs);
            packer.create_file(
                abs.to_string_lossy().into_owned(),
                rel.to_string_lossy().into_owned(),
            );
        }
        let mut buf = Vec::new();
        packer
            .pack(&mut buf)
            .context("pack managed input content")?;
        Ok(Box::new(std::io::Cursor::new(buf)))
    }

    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        let root = self.unpack_root.clone();
        let iter = self.files()?.into_iter().map(move |abs| {
            let rel = abs.strip_prefix(&root).unwrap_or(&abs).to_path_buf();
            let meta = std::fs::symlink_metadata(&abs)
                .with_context(|| format!("stat input file {}", abs.display()))?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&abs)
                    .with_context(|| format!("readlink {}", abs.display()))?;
                return Ok(WalkEntry {
                    path: rel,
                    kind: WalkEntryKind::Symlink { target },
                });
            }
            let x = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    false
                }
            };
            let f = std::fs::File::open(&abs).with_context(|| format!("open {}", abs.display()))?;
            Ok(WalkEntry {
                path: rel,
                kind: WalkEntryKind::File {
                    data: Box::new(f),
                    x,
                },
            })
        });
        Ok(Box::new(iter))
    }

    fn hashout(&self) -> Result<String> {
        // The hashout isn't carried on the run wire; inputs are addressed by path
        // here, not by content hash.
        Ok(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read as _, Write as _};

    // A managed input's content is readable from the files the host materialized
    // under unpack_root, scoped to this input's files via the list file.
    #[test]
    fn disk_input_content_walks_and_tars_listed_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("a.txt"), b"alpha").expect("write a");
        std::fs::write(root.join("sub/b.txt"), b"beta").expect("write b");
        // A sibling file NOT in the list must be excluded (proves per-input scoping).
        std::fs::write(root.join("other.txt"), b"nope").expect("write other");

        let list = root.join("input.list");
        {
            let mut f = std::fs::File::create(&list).expect("list");
            writeln!(f, "{}", root.join("a.txt").display()).expect("w");
            writeln!(f, "{}", root.join("sub/b.txt").display()).expect("w");
        }

        let content = DiskInputContent {
            unpack_root: root.clone(),
            list_path: Some(list),
        };

        // walk(): exactly the listed files, with their bytes, relative to root.
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for e in content.walk().expect("walk") {
            let mut e = e.expect("entry");
            if let WalkEntryKind::File { data, .. } = &mut e.kind {
                let mut s = String::new();
                data.read_to_string(&mut s).expect("read");
                seen.insert(e.path.to_string_lossy().into_owned(), s);
            }
        }
        assert_eq!(seen.get("a.txt").map(String::as_str), Some("alpha"));
        assert_eq!(seen.get("sub/b.txt").map(String::as_str), Some("beta"));
        assert!(!seen.contains_key("other.txt"), "must scope to the list");

        // reader(): a tar of the same files.
        let mut buf = Vec::new();
        content
            .reader()
            .expect("reader")
            .read_to_end(&mut buf)
            .expect("read tar");
        let entries: BTreeMap<String, String> =
            hcore::hartifactcontent::tar::TarWalker::new(std::io::Cursor::new(buf))
                .expect("tar walker")
                .map(|e| {
                    let mut e = e.expect("tar entry");
                    let mut s = String::new();
                    if let WalkEntryKind::File { data, .. } = &mut e.kind {
                        data.read_to_string(&mut s).expect("read tar file");
                    }
                    (e.path.to_string_lossy().into_owned(), s)
                })
                .collect();
        assert_eq!(entries.get("a.txt").map(String::as_str), Some("alpha"));
        assert_eq!(entries.get("sub/b.txt").map(String::as_str), Some("beta"));
        assert!(!entries.contains_key("other.txt"));
    }
}
