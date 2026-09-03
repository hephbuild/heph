//! The builder-platform probe, as a target.
//!
//! `docker_build` with no explicit `platforms` builds whatever the buildx builder
//! defaults to. Nothing else in the cache key varies by platform — the remote
//! key is `package/name/hashin` with no arch segment — so that default has to
//! reach the key, or an arm64 laptop and an amd64 runner compute the same key
//! for different image bytes and trade wrong-architecture artifacts through the
//! shared cache.
//!
//! This makes the answer a **target**: `//@heph/oci:platform` runs
//! `docker buildx inspect --bootstrap` and writes the platform it reports. An
//! `docker_build` that needs it depends on it, so the platform enters the image's
//! key as an ordinary input hash rather than as a parse-time side effect. It is
//! visible in `heph inspect deps`, single-flighted across every image target in
//! a run, and its subprocess is a normal target execution with the cancellation
//! and log handling every other target gets.
//!
//! # Cached locally, never remotely
//!
//! `cache = {enabled: True, remote: False}`. Local caching is what makes a warm
//! run cost nothing — without it the probe re-runs on every invocation, since an
//! uncached dep must execute to produce the hashout its consumer needs.
//!
//! Remote caching would be a bug: this is host state. Publishing it would let one
//! machine's answer serve another's, which is exactly the cross-machine mix-up
//! the probe exists to prevent. Each machine keeps its own answer, so each
//! machine's images key on their own platform — while the *images* themselves
//! still share the remote cache, correctly keyed.
//!
//! # When it re-probes
//!
//! The env vars that select which daemon and which builder answer are hashed
//! into the def (see [`ENV_KEYS`]), so switching any of them re-probes. So does
//! naming a different `builder`, which rides the target's address.
//!
//! What it does **not** notice is `docker buildx use <other>`, which changes the
//! default builder through docker's own state with no env var moving. The result
//! is a sticky default, not a wrong artifact: the recorded platform is passed
//! explicitly as `--platform`, so the image heph builds and the key it files it
//! under agree. Set `platforms` explicitly to opt out of the probe entirely.

use anyhow::Context as _;
use async_trait::async_trait;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hmodel::htpkg::PkgBuf;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{CacheConfig, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse,
};
use hplugin::provider::{
    ConfigRequest as ProviderConfigRequest, ConfigResponse as ProviderConfigResponse, GetError,
    GetRequest, GetResponse, ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse,
    ProbeRequest, ProbeResponse, Provider as EProvider, TargetSpec,
};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// The virtual package the probe target lives in.
pub const PKG: &str = "@heph/oci";
/// The probe target's name. A `builder` addr arg selects a non-default builder.
pub const TARGET: &str = "platform";
/// Addr arg naming the buildx builder to ask. Absent means the current one.
pub const BUILDER_ARG: &str = "builder";
pub const DRIVER_NAME: &str = "oci_builder_platform";

/// The shared registry blob store's target name, in [`PKG`].
///
/// `oci_pull` caches per target, so two images sharing a base layer download it
/// once *per target*. Registry blobs are addressed by digest, so one store can
/// serve every pull: an entry either matches its digest or is not used.
pub const BLOBS_TARGET: &str = "blobs";

/// Where the blob store mounts in a pulling target's sandbox.
const BLOBS_MOUNT: &str = ".heph-oci-blobs";

/// Environment variable the blob store's directory is announced through.
pub const BLOBS_ENV: &str = "HEPH_OCI_BLOBS";

/// The blob store's address, as written in a dep.
pub(crate) fn blobs_addr() -> String {
    format!("//{PKG}:{BLOBS_TARGET}")
}

/// The scratch reference every pulling target carries.
///
/// `hashed: false, runtime: false` — it materializes no artifacts and must not
/// reach the target's cache key: a pull returns the same image whether the store
/// was warm or empty.
pub(crate) fn blobs_input() -> hplugin::driver::targetdef::Input {
    hplugin::driver::targetdef::Input {
        r#ref: hplugin::driver::TargetAddr::parse(&blobs_addr(), &PkgBuf::from(""))
            .expect("the blob store addr is a constant and always parses"),
        mode: hplugin::driver::targetdef::InputMode::Standard,
        origin_id: format!("{}|0", hdriver_support::scratch::SCRATCH_ORIGIN_PREFIX),
        annotations: std::collections::BTreeMap::from([(
            hdriver_support::scratch::SCRATCH_ANNOTATION.to_string(),
            "true".to_string(),
        )]),
        hashed: false,
        runtime: false,
    }
}

/// The file the probe writes, relative to the workspace root.
const OUT_PATH: &str = "@heph/oci/platform.txt";

/// Host env vars that decide which daemon and builder answer the probe, hashed
/// into the def so changing any of them re-probes rather than serving a stale
/// answer. Mirrors what `exec`'s `pass_env` would do for a shell version of this
/// target.
const ENV_KEYS: &[&str] = &[
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "BUILDX_BUILDER",
    "DOCKER_CONFIG",
];

/// The address of the probe target for `builder`, as written in a dep.
pub(crate) fn addr_for(builder: Option<&str>) -> String {
    match builder {
        Some(b) => format!("//{PKG}:{TARGET}@{BUILDER_ARG}={b}"),
        None => format!("//{PKG}:{TARGET}"),
    }
}

/// Declares `//@heph/oci:platform`. Nothing else — the `oci_*` drivers are
/// selected from BUILD files by the workspace's own provider.
pub struct Provider;

impl EProvider for Provider {
    fn config(&self, _req: ProviderConfigRequest) -> anyhow::Result<ProviderConfigResponse> {
        Ok(ProviderConfigResponse {
            name: "pluginoci".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        // Deliberately not listed: the probe is an implementation detail an
        // `docker_build` depends on, not something a user builds by name, and it
        // would otherwise show up in every `heph query //...`.
        Box::pin(async move {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let items: Vec<anyhow::Result<ListPackageResponse>> = vec![Ok(ListPackageResponse {
                pkg: PkgBuf::from(PKG),
            })];
            Ok(Box::new(items.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            if req.addr.package != PKG {
                return Err(GetError::NotFound);
            }
            // The shared blob store. A `scratch` declaration, not a buildable
            // target: the engine resolves, locks and mounts the directory, and
            // `oci_pull` writes registry blobs into it instead of into a
            // per-sandbox directory that is thrown away with the sandbox.
            if req.addr.name == BLOBS_TARGET {
                return Ok(GetResponse {
                    target_spec: TargetSpec {
                        addr: req.addr,
                        driver: "scratch".to_string(),
                        config: std::collections::HashMap::from([
                            (
                                "path".to_string(),
                                hcore::htvalue::Value::String(BLOBS_MOUNT.to_string()),
                            ),
                            (
                                "env".to_string(),
                                hcore::htvalue::Value::String(BLOBS_ENV.to_string()),
                            ),
                            // A digest-addressed store is safe under concurrent
                            // writers for the same reason Go's build cache is:
                            // an entry either matches its digest or is not used,
                            // so two pulls racing on one blob cannot produce a
                            // wrong answer.
                            (
                                "access".to_string(),
                                hcore::htvalue::Value::String("shared".to_string()),
                            ),
                            // The *image* is platform-specific; the *blob store*
                            // is not. Blobs are opaque digest-named bytes, so one
                            // store can hold amd64 and arm64 layers side by side
                            // and travel between machines unchanged — hence no
                            // `version`, which is what declares a slot portable.
                            ("remote".to_string(), hcore::htvalue::Value::Bool(false)),
                        ]),
                        ..Default::default()
                    },
                });
            }
            if req.addr.name != TARGET {
                return Err(GetError::NotFound);
            }
            Ok(GetResponse {
                target_spec: TargetSpec {
                    addr: req.addr,
                    driver: DRIVER_NAME.to_string(),
                    ..Default::default()
                },
            })
        })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PlatformDef {
    /// The builder to ask, from the addr arg. `None` is buildx's current one.
    builder: Option<String>,
}

pub struct Driver {
    docker_bin: String,
}

impl Default for Driver {
    fn default() -> Self {
        Driver::new()
    }
}

impl Driver {
    pub fn new() -> Self {
        Driver::with_binary("docker")
    }

    /// Point the driver at a different binary. Public so tests can substitute a
    /// fake without a daemon.
    pub fn with_binary(bin: impl Into<String>) -> Self {
        Driver {
            docker_bin: bin.into(),
        }
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        hplugin::driver::DriverSchema::default()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let builder = addr.args.get(BUILDER_ARG).cloned();

        // The env is read here, not in `run`: it is an *input*, and hashing it is
        // what makes switching daemon or builder re-probe instead of serving the
        // previous machine state's answer.
        let mut h = Xxh3::new();
        h.update(b"oci_builder_platform/v1");
        if let Some(b) = &builder {
            h.update(b.as_bytes());
        }
        for key in ENV_KEYS {
            h.update(key.as_bytes());
            if let Ok(v) = std::env::var(key) {
                h.update(v.as_bytes());
            }
        }
        let hash = format!("{:x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(PlatformDef { builder }),
                inputs: vec![],
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![OutPath {
                        content: Content::FilePath(OUT_PATH.to_string()),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                support_files: vec![],
                // Local yes, remote never — see the module header.
                cache: CacheConfig::on(false),
                pty: false,
                hash,
                transparent: false,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<PlatformDef>().clone();
        let out = req.sandbox_pkg_dir.join("platform.txt");
        let cwd = req.sandbox_ws_dir.clone();

        let mut argv = vec![
            self.docker_bin.clone(),
            "buildx".to_string(),
            "inspect".to_string(),
            "--bootstrap".to_string(),
        ];
        if let Some(builder) = &def.builder {
            argv.push(builder.clone());
        }

        let mut io = super::docker_build::ToolIo::from_request(&mut req.request);
        let stdout = super::docker_build::run_tool(
            argv,
            &cwd,
            "docker buildx inspect",
            &mut io,
            ctoken,
        )
        .await
        .with_context(|| {
            let which = def.builder.as_deref().map_or_else(
                || "the current builder".to_string(),
                |b| format!("builder {b:?}"),
            );
            format!(
                "asking {which} for its default platform — a `docker_build` with no `platforms` \
                     needs it for the cache key. Set `platforms` explicitly to skip this probe."
            )
        })?;

        let platform = super::docker_build::parse_builder_platform(&stdout)?;
        tokio::fs::write(&out, &platform)
            .await
            .with_context(|| format!("write platform {out:?}"))?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    /// The `blobs` spec must survive the driver that will actually parse it.
    ///
    /// A `scratch` declaration rejects unknown fields, so emitting one is not a
    /// warning — it is a hard failure of every target that references the cache.
    /// This crate shipped exactly that: it kept setting `platform` after the
    /// field was removed from the declaration, and nothing here noticed because
    /// no test parsed the spec it builds. `oci_docker`'s e2e caught it, which is
    /// three minutes of CI later than it needed to be.
    #[tokio::test]
    async fn the_blobs_spec_parses_as_a_declaration() {
        let addr = hmodel::htaddr::parse_addr(&blobs_addr()).expect("addr");
        let resp = Provider
            .get(
                GetRequest {
                    request_id: "req".to_string(),
                    addr,
                    states: vec![],
                    executor: std::sync::Arc::new(hplugin::provider::NoopExecutor),
                },
                &StdCancellationToken::new(),
            )
            .await
            .expect("the provider serves the blob store");
        hbuiltins::pluginscratch::parse_declaration(&resp.target_spec)
            .expect("the blobs spec must parse as a scratch declaration");
    }

    #[test]
    fn addr_carries_the_builder_as_an_arg() {
        assert_eq!(addr_for(None), "//@heph/oci:platform");
        assert_eq!(
            addr_for(Some("multi")),
            "//@heph/oci:platform@builder=multi"
        );
    }

    async fn parse(addr: &str) -> ParseResponse {
        let addr = hmodel::htaddr::parse_addr(addr).expect("addr");
        Driver::new()
            .parse(
                ParseRequest {
                    request_id: "req".to_string(),
                    target_spec: Arc::new(TargetSpec {
                        addr,
                        driver: DRIVER_NAME.to_string(),
                        config: HashMap::new(),
                        labels: vec![],
                        ..Default::default()
                    }),
                },
                &StdCancellationToken::new(),
            )
            .await
            .expect("parse")
    }

    /// Local caching is what makes a warm run free — without it the probe reruns
    /// on every invocation. Remote caching would be a bug: this is host state,
    /// and publishing it would let one machine's answer serve another's, which
    /// is the cross-machine mix-up the probe exists to prevent.
    #[tokio::test]
    async fn cached_locally_never_remotely() {
        let cache = parse("//@heph/oci:platform").await.target_def.cache;
        assert!(cache.enabled, "a warm run must not re-probe");
        assert!(
            !cache.remote_enabled,
            "the platform is host state and must never be published"
        );
    }

    /// Switching daemon or builder must re-probe rather than serve the previous
    /// machine state's answer, so the selecting env vars are hashed into the def.
    #[tokio::test]
    async fn the_selecting_env_is_in_the_key() {
        // The var is restored before returning; `set_var` is unsafe only
        // because another thread could observe the change mid-flight.
        let before = std::env::var("DOCKER_HOST").ok();
        unsafe { std::env::set_var("DOCKER_HOST", "unix:///one.sock") };
        let a = parse("//@heph/oci:platform").await.target_def.hash;
        unsafe { std::env::set_var("DOCKER_HOST", "unix:///two.sock") };
        let b = parse("//@heph/oci:platform").await.target_def.hash;
        match before {
            Some(v) => unsafe { std::env::set_var("DOCKER_HOST", v) },
            None => unsafe { std::env::remove_var("DOCKER_HOST") },
        }
        assert_ne!(a, b, "a different daemon must re-probe");
    }

    /// Two builders answer with two different default platforms, so they must
    /// not share one cached answer.
    #[tokio::test]
    async fn a_named_builder_keys_apart() {
        let default = parse("//@heph/oci:platform").await.target_def.hash;
        let named = parse("//@heph/oci:platform@builder=multi")
            .await
            .target_def
            .hash;
        assert_ne!(default, named);
    }
}
