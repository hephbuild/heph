//! The `oci_load` driver: loads an image archive (produced by a `docker_build`
//! target) into the local docker daemon.
//!
//! An *action*, not an artifact: it mutates the host daemon's image store and is
//! therefore **not cached** — it runs every time.
//!
//! Talks to the daemon's API directly (`bollard`, `/images/load`), so there is
//! no skopeo and no `docker` CLI on the path. What skopeo used to do invisibly
//! and is done here instead is the *conversion*: `docker load` accepts an OCI
//! archive only on a daemon with the containerd image store, which is still not
//! the default everywhere, so the image is rewritten into a docker-format
//! archive with uncompressed layers first (see
//! [`super::archive::write_docker_archive`]).
//!
//! # Tagging
//!
//! With no `tag`, the image is tagged by **content**: the image target's address
//! as the repository, this load's input hash as the tag.
//!
//! ```console
//! $ heph run //app:load
//! app_img:9f2c4e1b7a0d3856
//! ```
//!
//! A default is possible here at all because the useful property is not a name,
//! it is *which image is this*. A tag written in a BUILD file is a moving
//! target — it says which image was loaded last, so two branches, two people, or
//! a rebuild after a change all collide on it and `docker run app:dev` silently
//! runs the wrong thing. The derived one cannot: it changes when and only when
//! the image changes, two people on the same commit get the same string, and it
//! can be predicted from the graph rather than read out of a log.
//!
//! `tag` overrides it, for when a human has to type something.
//!
//! A daemon tag holds one image, so loading a **multi-platform** archive means
//! choosing an instance: `platform` (default Linux on the host's architecture).
//! The selection happens here, while assembling the archive, rather than being
//! delegated to a tool whose own default is the host's GOOS — `darwin` on a Mac,
//! which no Linux manifest list contains.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::{archive::Layout, dep_single_file};

pub const DRIVER_NAME: &str = "oci_load";

const IMAGE_ORIGIN: &str = "image";

/// Config for an `oci_load` target.
#[derive(Spec)]
struct OciLoadSpec {
    /// Target address of the image to load — a `docker_build` target. Only its
    /// archive output (group `""`) is consumed.
    #[spec(required)]
    image: String,
    /// Local tag to give the loaded image, e.g. `app:dev`.
    ///
    /// Optional. Left unset, the tag is derived from the image target and this
    /// load's input hash:
    ///
    /// ```console
    /// $ heph run //app:load
    /// app_img:9f2c4e1b7a0d3856
    /// ```
    ///
    /// The derived one is what anything automated wants: it names exactly what
    /// was built, it changes when and only when the image changes, and two
    /// people running the same commit get the same string. A name written here
    /// can do none of that — it says which image was loaded *last*, not which
    /// one is which — so set it only when a human has to type it.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    tag: Option<String>,
    /// Which instance to load out of a **multi-platform** archive, as `os/arch`
    /// (e.g. `linux/amd64`). Defaults to Linux on the host's architecture.
    ///
    /// A daemon holds one image per tag, so a multi-arch archive built by
    /// `docker_build(platforms = [...])` must be narrowed to one instance on the way
    /// in. Left to skopeo's own default the choice would follow the host's
    /// GOOS/GOARCH — `darwin` on macOS, which no Linux manifest list matches, so
    /// the load would fail outright there.
    ///
    /// `tool = "docker"` rejects it: `docker load` has no instance selection.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    platform: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciLoadDef {
    /// The tag the BUILD file asked for. `None` derives one from `repo` and the
    /// input hash at run time.
    tag: Option<String>,
    /// Docker repository name derived from the **image** target's address —
    /// `//cmd/server:img` becomes `cmd_server_img`.
    ///
    /// The image target's, not this load target's: the repository names what
    /// you are about to run, and nobody names their load target after the image
    /// it loads. Resolved at parse so a name that cannot be made into a legal
    /// repository is a BUILD-file error, not a surprise from the daemon.
    repo: String,
    /// The `os/arch` instance taken out of the archive — always concrete, never
    /// "whatever the host is": a multi-arch archive has no `darwin` instance to
    /// match on macOS.
    platform: String,
}

/// v2: the skopeo path pins the loaded instance instead of following the host's
/// GOOS/GOARCH.
/// v3: loaded through the daemon API, converting to a docker-format archive on
/// the way in; `tool` and `format` are gone, so neither is in the key.
/// v4: `tag` is optional and defaults to a content-addressed name, so the
/// derived repository is part of the def.
const OCI_LOAD_FORMAT_VERSION: u32 = 4;

impl Hash for OciLoadDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_LOAD_FORMAT_VERSION.hash(state);
        self.tag.hash(state);
        self.repo.hash(state);
        self.platform.hash(state);
    }
}

/// A docker repository name derived from a target address.
///
/// `//cmd/server:img` becomes `cmd_server_img`; `//app:img@v=linux_amd64`
/// becomes `app_img-v-linux-amd64`, so two variants of one target do not land
/// on one name.
///
/// **One component, joined by `_`** rather than a `/`-separated path. A
/// multi-component name invites docker to read the first one as a *registry
/// host* — it does exactly that whenever that component holds a `.` or a `:` —
/// so `example.com/svc/img` turns `docker run` into a network pull from a
/// registry that does not exist. A single component can never be read that way,
/// and it keeps `docker images` one flat row per target.
///
/// The reference grammar is narrow — a component is
/// `[a-z0-9]+([._]|__|[-]+[a-z0-9]+)*` — and a heph address is not, so every
/// character outside `[a-z0-9]` becomes `-` and each address segment is trimmed
/// of leading and trailing dashes before being joined. `_` therefore only ever
/// appears as this function's own separator, one at a time, between non-empty
/// segments: never the `___` run the grammar rejects.
fn docker_repo(addr: &hmodel::htaddr::Addr) -> anyhow::Result<String> {
    fn component(raw: &str) -> String {
        let mapped: String = raw
            .chars()
            .map(|c| {
                let c = c.to_ascii_lowercase();
                if c.is_ascii_alphanumeric() { c } else { '-' }
            })
            .collect();
        mapped.trim_matches('-').to_string()
    }

    let mut parts: Vec<String> = addr
        .package
        .as_str()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(component)
        .filter(|s| !s.is_empty())
        .collect();

    // Variants are different images and must not share a repository: the tag
    // distinguishes them by hash, but `docker images` would show two rows under
    // one name with nothing to tell them apart.
    let mut name = component(&addr.name);
    for (k, v) in &addr.args {
        name.push('-');
        name.push_str(&component(k));
        name.push('-');
        name.push_str(&component(v));
    }
    anyhow::ensure!(
        !name.is_empty(),
        "the image target's name {:?} has no character a docker repository name can hold \
         (it takes `a-z0-9`); give `oci_load` an explicit `tag`",
        addr.name
    );
    parts.push(name);

    let repo = parts.join("_");
    // The one name docker reads as a registry host without needing a `.` or a
    // `:`. Only reachable from a root-package target literally called
    // `localhost`, but the failure — `docker run` going to the network instead
    // of the image just loaded — is bad enough to be worth two lines.
    if repo == "localhost" {
        return Ok("heph_localhost".to_string());
    }
    Ok(repo)
}

/// Connect to the daemon the docker CLI would use.
///
/// `bollard` honours `$DOCKER_HOST` and otherwise assumes
/// `/var/run/docker.sock`. That is wrong on any machine using a docker
/// *context* — OrbStack, Colima, Rancher, a rootless Podman — where the default
/// socket is often a stale Docker Desktop one that accepts the connection and
/// then never answers. The symptom is not an error but a two-minute hang, so
/// this reads the current context's endpoint the way the CLI does.
fn connect_daemon() -> anyhow::Result<bollard::Docker> {
    if std::env::var_os("DOCKER_HOST").is_some() {
        // An explicit host wins, exactly as it does for the CLI.
        return bollard::Docker::connect_with_defaults()
            .context("connect to the docker daemon named by $DOCKER_HOST");
    }
    if let Some(host) = current_context_host() {
        return bollard::Docker::connect_with_socket(&host, 120, bollard::API_DEFAULT_VERSION)
            .with_context(|| format!("connect to the docker daemon at {host}"));
    }
    bollard::Docker::connect_with_local_defaults()
        .context("connect to the docker daemon (is it running?)")
}

/// The unix socket of the docker CLI's current context, if it names one.
///
/// Read from `~/.docker` rather than shelled out to: `docker context inspect`
/// is a process spawn on a path that runs for every load, and the file is the
/// same thing the CLI reads.
fn current_context_host() -> Option<String> {
    let home = match std::env::var_os("DOCKER_CONFIG") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".docker"),
    };
    let name = std::fs::read_to_string(home.join("config.json"))
        .ok()
        .and_then(|raw| {
            serde_json::from_str::<serde_json::Value>(&raw)
                .ok()?
                .get("currentContext")?
                .as_str()
                .map(str::to_string)
        })?;
    if name == "default" {
        return None;
    }
    // The CLI stores each context under the sha256 of its name.
    let digest = super::archive::sha256_digest(name.as_bytes());
    let hex = digest.strip_prefix("sha256:")?;
    let meta = home.join("contexts/meta").join(hex).join("meta.json");
    let raw = std::fs::read_to_string(meta).ok()?;
    let host = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("Endpoints")?
        .get("docker")?
        .get("Host")?
        .as_str()?
        .to_string();
    host.strip_prefix("unix://").map(str::to_string)
}

/// Stateless: the daemon connection is made per load from the ambient docker
/// environment, and there is no host binary left to point anywhere.
#[derive(Default)]
pub struct Driver;

impl Driver {
    pub fn new() -> Self {
        Driver
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
        OciLoadSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = OciLoadSpec::from(&req.target_spec.config).context("parse oci_load config")?;
        let platform =
            super::normalize_platform(&spec.platform.unwrap_or_else(super::default_platform))
                .context("`platform`")?;

        let mut image_ref = TargetAddr::parse(&spec.image, &addr.package)
            .with_context(|| format!("parse image ref {:?}", spec.image))?;
        super::pin_archive_group(&mut image_ref, &spec.image)?;

        let def = OciLoadDef {
            tag: spec.tag,
            repo: docker_repo(&image_ref.r#ref)?,
            platform,
        };
        let hash = {
            let mut h =
                DebugHasher::new(Xxh3Default::new(), || format!("oci_load_{}", addr.format()));
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: vec![Input {
                    r#ref: image_ref,
                    mode: InputMode::Standard,
                    origin_id: IMAGE_ORIGIN.to_string(),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                }],
                outputs: vec![],
                support_files: vec![],
                cache: CacheConfig::off(),
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
        req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciLoadDef>().clone();
        let path = dep_single_file(&req, IMAGE_ORIGIN)?;
        let layout =
            Layout::read(&path).with_context(|| format!("read the image to load from {path:?}"))?;

        // A daemon tag holds one image, so pick the instance here rather than
        // handing the daemon a manifest list it may or may not understand.
        let (manifest, manifest_digest) = super::select_platform(&layout, &def.platform)?;

        // Unset, the tag is derived: what was built, and nothing about when.
        // `hashin` is only known here — the engine computes it after `parse` —
        // so this is materialized at run time rather than carried in the def.
        let image = def
            .tag
            .clone()
            .unwrap_or_else(|| format!("{}:{}", def.repo, req.request.hashin));

        let docker_tar = req.sandbox_dir.join("oci-load-docker.tar");
        super::archive::write_docker_archive(
            &docker_tar,
            &layout,
            &manifest,
            &manifest_digest,
            &image,
        )
        .context("convert the image to a docker-format archive")?;

        let docker = connect_daemon()?;
        let bytes = tokio::fs::read(&docker_tar)
            .await
            .with_context(|| format!("read {docker_tar:?}"))?;

        use futures::StreamExt as _;
        let mut stream = docker.import_image(
            bollard::query_parameters::ImportImageOptions::default(),
            bollard::body_full(bytes.into()),
            None,
        );
        // The daemon narrates the load and names what it loaded; that name is
        // the only reliable handle on the new image. Its id is not the config
        // digest under the containerd image store, so guessing gets a 404.
        let mut narration = String::new();
        while let Some(item) = stream.next().await {
            let info = item.context("load image into the docker daemon")?;
            // A rejected image arrives in-band, as a frame on an otherwise
            // successful stream — not as a transport error.
            if let Some(err) = info.error_detail {
                anyhow::bail!(
                    "docker rejected the image: {}",
                    err.message.unwrap_or_default()
                );
            }
            if let Some(line) = info.stream {
                narration.push_str(&line);
            }
        }
        let loaded = super::parse_docker_load_ref(&narration)
            .context("the daemon accepted the archive but did not say what it loaded")?;

        // Tag explicitly rather than trusting `manifest.json`'s `RepoTags`: with
        // both an `index.json` and a `manifest.json` in the archive, a
        // containerd-backed daemon takes the OCI path and ignores RepoTags,
        // leaving a `<none>:<none>` image the user cannot run.
        //
        let (repo, tag) = image.rsplit_once(':').unwrap_or((image.as_str(), "latest"));
        docker
            .tag_image(
                &loaded,
                Some(bollard::query_parameters::TagImageOptions {
                    repo: Some(repo.to_string()),
                    tag: Some(tag.to_string()),
                }),
            )
            .await
            .with_context(|| format!("tag the loaded image as {image}"))?;

        // Logged, not printed: stdout belongs to the TUI. This is the line that
        // tells the user what to `docker run`.
        tracing::info!(image, platform = def.platform, "oci_load: loaded");
        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn parse_req(addr: &str, config: HashMap<String, Value>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr(addr).expect("addr"),
                driver: DRIVER_NAME.to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    fn cfg(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    async fn parse(addr: &str, config: HashMap<String, Value>) -> ParseResponse {
        Driver::new()
            .parse(parse_req(addr, config), &StdCancellationToken::new())
            .await
            .expect("parse")
    }

    /// An implicit load records a concrete instance, so a multi-arch archive
    /// resolves the same way on every host — and at all on macOS.
    #[tokio::test]
    async fn parse_defaults_platform_to_linux_host_arch() {
        let resp = parse(
            "//app:load",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("tag", Value::String("app:dev".to_string())),
            ]),
        )
        .await;
        let platform = resp.target_def.def::<OciLoadDef>().platform.clone();
        assert!(platform.starts_with("linux/"), "got: {platform}");
    }

    /// Loading the amd64 instance and loading the arm64 one are different
    /// actions; they must not collapse to one def.
    #[tokio::test]
    async fn parse_hash_differs_per_platform() {
        let base = |p: &str| {
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("tag", Value::String("app:dev".to_string())),
                ("platform", Value::String(p.to_string())),
            ])
        };
        let a = parse("//app:load", base("linux/amd64")).await;
        let b = parse("//app:load", base("linux/arm64")).await;
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// A repository name is built out of the *image* target's address, and the
    /// grammar it has to fit is much narrower than an address: lowercase
    /// alphanumerics and a few separators, per path component.
    #[test]
    fn repositories_are_derived_from_the_image_address() {
        let repo = |a: &str| docker_repo(&parse_addr(a).expect("addr")).expect("repo");

        assert_eq!(repo("//cmd/server:img"), "cmd_server_img");
        assert_eq!(repo("//:img"), "img");
        // Uppercase and underscores are not in the grammar.
        assert_eq!(repo("//Cmd/My_Server:Img"), "cmd_my-server_img");

        // A single component can never be read as a registry host — which is
        // the reason for joining with `_` rather than keeping the `/` path. A
        // `.` in the first component of a *path* makes docker treat it as a
        // hostname and `docker run` goes to the network.
        assert_eq!(repo("//example.com/svc:img"), "example-com_svc_img");
        // `localhost` is a host by name alone, but only when it is the whole
        // first component — which after joining it no longer is.
        assert_eq!(repo("//localhost:img"), "localhost_img");
        assert_eq!(repo("//:localhost"), "heph_localhost");

        // `_` is this function's separator and appears one at a time: the
        // grammar allows `_` and `__` between alphanumerics but not `___`.
        assert!(
            !repo("//a/_/b:_c_").contains("___"),
            "a run of three underscores is not a legal repository name"
        );
    }

    /// Two variants of one target are two different images. They must not share
    /// a repository, or `docker images` shows two rows under one name with
    /// nothing to tell them apart.
    #[test]
    fn variants_get_their_own_repository() {
        let repo = |a: &str| docker_repo(&parse_addr(a).expect("addr")).expect("repo");
        assert_eq!(repo("//app:img@v=linux_amd64"), "app_img-v-linux-amd64");
        assert_ne!(
            repo("//app:img@v=linux_amd64"),
            repo("//app:img@v=linux_arm64")
        );
    }

    /// A name with nothing a repository can hold fails in `parse`, naming the
    /// way out — not later, from the daemon, as a reference-format error.
    #[test]
    fn an_unrepresentable_name_is_a_build_file_error() {
        let addr = parse_addr("//app:___").expect("addr");
        let err = format!("{:#}", docker_repo(&addr).expect_err("unrepresentable"));
        assert!(err.contains("explicit `tag`"), "got: {err}");
    }

    /// `tag` is optional now: one is derived, so an `oci_load` without it is a
    /// complete target rather than one that leaves a dangling `<none>:<none>`.
    #[tokio::test]
    async fn parse_needs_no_tag() {
        let resp = parse(
            "//app:load",
            cfg(&[("image", Value::String(":img".to_string()))]),
        )
        .await;
        let def = resp.target_def.def::<OciLoadDef>();
        assert_eq!(
            def.repo, "app_img",
            "the repo follows the image, not the load"
        );
        assert!(def.tag.is_none());
    }

    /// The tag is part of the def: naming the image differently is a different
    /// action, and `oci_load` is uncached precisely because its effect is on the
    /// daemon.
    #[tokio::test]
    async fn an_extra_tag_changes_the_def() {
        let bare = parse(
            "//app:load",
            cfg(&[("image", Value::String(":img".to_string()))]),
        )
        .await;
        let tagged = parse(
            "//app:load",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("tag", Value::String("app:dev".to_string())),
            ]),
        )
        .await;
        assert_ne!(bare.target_def.hash, tagged.target_def.hash);
    }

    /// Two load targets pointing at *different* images must not tag one
    /// repository — the whole point is that the name says what it is.
    #[tokio::test]
    async fn the_repo_follows_which_image_is_loaded() {
        let a = parse(
            "//app:load",
            cfg(&[("image", Value::String("//svc/api:img".to_string()))]),
        )
        .await;
        let b = parse(
            "//app:load",
            cfg(&[("image", Value::String("//svc/web:img".to_string()))]),
        )
        .await;
        assert_eq!(a.target_def.def::<OciLoadDef>().repo, "svc_api_img");
        assert_eq!(b.target_def.def::<OciLoadDef>().repo, "svc_web_img");
    }

    /// Handing skopeo the `digest` group would give it a text file where it
    /// expects an archive.
    #[tokio::test]
    async fn parse_rejects_an_explicit_output_group() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:load",
                    cfg(&[
                        ("image", Value::String(":img|digest".to_string())),
                        ("tag", Value::String("t".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("an explicit group must fail");
        assert!(format!("{err:#}").contains("digest"), "got: {err:#}");
    }
}
