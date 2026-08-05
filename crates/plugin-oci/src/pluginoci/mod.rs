//! The `oci_image` driver: builds a container image archive from a Dockerfile +
//! build context into a cacheable target output.
//!
//! One target = one built image, materialized as a single archive file (`oci` or
//! `docker` format) plus a `digest` output group carrying the image digest as
//! plain text, so a downstream target (a deploy manifest, a compose file, an
//! `exec` target that records what was built) can read it without unpacking the
//! archive. `oci_push` / `oci_load` deliberately do *not* read it — they consume
//! the archive group — so the digest group exists for the graph, not for them.
//!
//! Caching is layered:
//!  1. **heph input-hash cache** (the big win): the build context files,
//!     Dockerfile, build args, build stage, platforms and base-image contexts
//!     are the target's inputs — an unchanged context is a cache hit and the
//!     image is *not* rebuilt, the archive is served from the local or remote
//!     cache. Image nondeterminism (timestamps) does not defeat this: heph keys
//!     on inputs and serves the identical cached archive to consumers.
//!  2. **BuildKit layer cache** when a build does run: `cache_from` / `cache_to`
//!     wire `docker buildx --cache-from/--cache-to` (registry or inline refs).
//!     These select where layers may be *reused from*; they change how long a
//!     build takes, and can change the exported layer digests, but never what
//!     the image does — so they are deliberately excluded from the input hash,
//!     and on a heph cache hit no build runs and they have no effect at all.
//!
//! # What is NOT an input
//!
//! The list above is not exhaustive over everything that can change the image.
//! These are known, deliberate exemptions — the same class as `http_fetch`
//! without a `sha256` — and each one means two machines can compute the same
//! cache key for different bytes:
//!
//! - **`FROM` base images.** BuildKit resolves them itself, from the network,
//!   against its own content store. `FROM alpine:3.20` is whatever that tag
//!   pointed at when the machine that populated the cache last resolved it.
//!   Pin `FROM` by `@sha256:`, or produce the base with an `oci_pull` target and
//!   reference it through `bases`, which *is* a hashed input.
//! - **Anything `RUN` fetches** — `apt-get`, `curl`, `npm ci`. Execution-time
//!   downloads are neither content-addressed nor verified.
//! - **Secret and SSH *values*.** The spec strings are hashed; what the agent or
//!   the environment hands the build is not, and cannot be (it would land in the
//!   `HEPH_DEBUG_HASH` trace).
//! - **The host `docker` / `buildx` / `skopeo` version.** BuildKit changes
//!   output-visible defaults across releases (attestations, compression), so two
//!   machines on different versions can produce structurally different archives
//!   under one key. Pinning the toolchain is the end state; the host is what
//!   ships today.
//! - **`.dockerignore`.** Nothing stages one, so unless a `context` target
//!   produces it the build sees none — a heph-built image can therefore differ
//!   from the same `docker build` run by hand in the repo.
//!
//! The environment is *not* on this list: every subprocess runs with a cleared
//! environment and an explicit passthrough allowlist (see
//! [`passthrough_oci_env`]), so a stray `BUILDX_BUILDER` or `SOURCE_DATE_EPOCH`
//! cannot change the build behind the key's back.
//!
//! # The builder
//!
//! The builder is host `docker buildx` (a host capability, like `http_fetch`'s
//! network); a hermetic toolchain can replace it later without changing targets.
//!
//! It must be one that can **write an image archive to a file**. The plain
//! `docker` driver — what a stock Docker Engine selects by default — cannot: it
//! only loads or pushes into the daemon, so both `--output type=oci,dest=…` and
//! `type=docker,dest=…` fail on it, and with them every `oci_image` target
//! whatever its `format`. Either turn on the daemon's containerd image store, or
//! create a container builder and name it:
//!
//! ```console
//! $ docker buildx create --name heph --driver docker-container
//! ```
//!
//! ```python
//! oci_image(name = "img", builder = "heph", ...)
//! ```
//!
//! Which builder runs is a hashed input (`builder`), which is why
//! `BUILDX_BUILDER` is stripped from the build's environment.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::{Spec, TargetSpecCache};
use hproc::proc_exec;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

/// The user-facing output streams a driver relays its tool's output to.
///
/// Borrowed from `RunRequest` and threaded through every tool invocation, so a
/// multi-command driver (`oci_push`'s load → tag → push) writes all three
/// children's output to the same place, in order.
pub(crate) struct ToolIo<'io> {
    pub stdout: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    pub stderr: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    /// The target's `log.txt`, in the sandbox dir.
    ///
    /// This is what makes a build's progress *visible*: the engine collects that
    /// file as the target's output artifact, renders its tail in the failure box
    /// and serves it to `heph log`. The `stdout`/`stderr` sinks above are the
    /// live path and are `None` outside an interactive run, so without this a
    /// `docker buildx build` printed its whole progress log nowhere at all.
    log: Option<std::sync::Mutex<std::fs::File>>,
}

/// The three places a tool's output goes: the user's live stdout/stderr (absent
/// outside an interactive run) and the target's log file.
///
/// `&mut dyn Trait` is invariant, so the trait objects' lifetime has to stay
/// `'io` rather than being shortened to the reborrow's.
type Sinks<'a, 'io> = (
    Option<&'a mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin + 'io)>,
    Option<&'a mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin + 'io)>,
    Option<&'a std::sync::Mutex<std::fs::File>>,
);

impl<'io> ToolIo<'io> {
    /// Take the sinks out of a run request. They are `&mut` and single-owner, so
    /// this consumes them for the duration of the run.
    pub(crate) fn from_request<'a>(req: &mut hplugin::driver::RunRequest<'a, 'io>) -> Self {
        // Appended, not truncated: one run may shell out several times
        // (`oci_push` does load → tag → push → rmi) and each one's output
        // belongs to the same target's log.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(req.sandbox_dir.join("log.txt"))
            .map_err(|e| {
                // Losing the log must not fail the build; say so once and carry
                // on with whatever live sinks exist.
                tracing::warn!(error = %e, "oci: could not open the target's log.txt");
            })
            .ok()
            .map(std::sync::Mutex::new);
        ToolIo {
            stdout: req.stdout.take(),
            stderr: req.stderr.take(),
            log,
        }
    }

    /// Reborrow both sinks at once. Split out as a method so the two disjoint
    /// field borrows come from a single `&mut self`, which the borrow checker
    /// accepts where two separate `io.stdout` / `io.stderr` reborrows would not.
    fn sinks(&mut self) -> Sinks<'_, 'io> {
        let ToolIo {
            stdout,
            stderr,
            log,
        } = self;
        (stdout.as_deref_mut(), stderr.as_deref_mut(), log.as_ref())
    }
}

pub const DRIVER_NAME: &str = "oci_image";

pub mod archive;
pub mod load;
pub mod platform;
pub mod pull;
pub mod push;
pub mod registry;

/// Archive format an image is built/consumed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ImageFormat {
    /// OCI image layout archive (`--output type=oci`). Portable, standard;
    /// pushed/loaded daemonlessly with `skopeo`.
    Oci,
    /// Docker-format image archive (`--output type=docker`). Loadable straight
    /// into a docker daemon with `docker load`.
    Docker,
}

impl ImageFormat {
    /// The BuildKit `--output type=` value.
    fn output_type(self) -> &'static str {
        match self {
            ImageFormat::Oci => "oci",
            ImageFormat::Docker => "docker",
        }
    }

    /// The containers/image transport name for reading this archive as a source
    /// (skopeo `<transport>:<path>`).
    pub(crate) fn transport(self) -> &'static str {
        match self {
            ImageFormat::Oci => "oci-archive",
            ImageFormat::Docker => "docker-archive",
        }
    }

    pub(crate) fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "oci" => Ok(ImageFormat::Oci),
            "docker" => Ok(ImageFormat::Docker),
            other => anyhow::bail!("`format` must be \"oci\" or \"docker\", got {other:?}"),
        }
    }
}

/// The host CLIs these drivers shell out to.
///
/// One struct rather than two adjacent `String` parameters: the drivers had
/// already drifted into `(skopeo, docker)` in one file and `(docker, skopeo)` in
/// another, and swapping two same-typed arguments compiles and passes.
#[derive(Clone, Debug)]
pub struct Tools {
    pub docker: String,
    pub skopeo: String,
}

impl Default for Tools {
    fn default() -> Self {
        Tools {
            docker: "docker".to_string(),
            skopeo: "skopeo".to_string(),
        }
    }
}

/// The CLI used to move an image between an archive, a registry, and the local
/// daemon (for `oci_push` / `oci_pull` / `oci_load`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Tool {
    /// `skopeo` — daemonless, reads/writes both OCI and docker archives.
    Skopeo,
    /// The `docker` CLI — needs the daemon and only handles docker-format
    /// archives (`docker load`/`save`/`push`), but keeps `skopeo` off the
    /// dependency list.
    Docker,
}

impl Tool {
    /// Resolve the `tool` config value. Absent picks per format so `skopeo` is
    /// only required for OCI archives: a docker-format image uses the `docker`
    /// CLI, an OCI-format image uses `skopeo`.
    pub(crate) fn parse_opt(s: Option<&str>, format: ImageFormat) -> anyhow::Result<Self> {
        match s {
            None => Ok(match format {
                ImageFormat::Docker => Tool::Docker,
                ImageFormat::Oci => Tool::Skopeo,
            }),
            Some("skopeo") => Ok(Tool::Skopeo),
            Some("docker") => Ok(Tool::Docker),
            Some(other) => {
                anyhow::bail!("`tool` must be \"skopeo\" or \"docker\", got {other:?}")
            }
        }
    }

    /// Stable label for hashing.
    fn label(self) -> &'static str {
        match self {
            Tool::Skopeo => "skopeo",
            Tool::Docker => "docker",
        }
    }
}

/// The `docker` CLI cannot read an OCI archive (`docker load`/`save`/`push` are
/// docker-format only). Reject that combination at parse time with a clear
/// message rather than a cryptic runtime failure.
pub(crate) fn ensure_tool_supports_format(tool: Tool, format: ImageFormat) -> anyhow::Result<()> {
    if tool == Tool::Docker && format == ImageFormat::Oci {
        anyhow::bail!(
            "tool=\"docker\" cannot handle an `oci` archive; build with format=\"docker\" or use \
             tool=\"skopeo\""
        );
    }
    Ok(())
}

/// Config for an `oci_image` target.
#[derive(Spec)]
struct OciImageSpec {
    /// The Dockerfile, as either a **target address** or a path. Default
    /// `Dockerfile`.
    ///
    /// An address — anything starting with `:` or `//`, e.g. `":dockerfile"` or
    /// `"//base:Dockerfile"` — makes the target producing it a dep of this one,
    /// staged and hashed on its own. That is the form to reach for when the
    /// Dockerfile is generated, or lives in another package: no `context` entry
    /// and no path spelling, and the target that produces it is what the cache
    /// key follows.
    ///
    /// ```python
    /// oci_image(name = "img", dockerfile = ":dockerfile", context = [":srcs"])
    /// ```
    ///
    /// A path is relative to the target's package, and names a file some
    /// `context` dep must materialize (a plain checked-in `Dockerfile` comes
    /// from the `fs` provider). A relative path may reach into a sibling package
    /// (`../base/Dockerfile`); absolute paths are rejected, since a host file
    /// outside the sandbox is not a declared input and its edits would never
    /// invalidate the cache.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    dockerfile: Option<String>,
    /// Build-context dependencies, grouped by name → list of target addresses.
    ///
    /// These, and only these, are what the build sees: heph's own deps (the
    /// builder-platform probe, `bases` layouts) and the target's own output
    /// archive are all kept out of the context deliberately.
    /// Every file these targets produce is materialized into the sandbox at its
    /// **workspace-relative** path, and the sandbox workspace root is the
    /// `docker build` context — so a dep from any package is visible, and
    /// `COPY` paths are workspace-relative (`COPY cmd/server/bin /usr/bin/`).
    /// These are hashed inputs: an unchanged context is a cache hit (no rebuild).
    ///
    /// Each group is also exported as a `SRC_<GROUP>` build arg holding the
    /// context-relative paths that group produced, so a Dockerfile can consume
    /// it without hardcoding layout:
    ///
    /// ```dockerfile
    /// ARG SRC_BIN
    /// COPY ${SRC_BIN} /usr/bin/server
    /// ```
    ///
    /// The default (unnamed) group is `SRC`. Group names are uppercased and
    /// non-alphanumerics become `_`, matching `exec`'s `SRC_*` convention.
    context: HashMap<String, Vec<String>>,
    /// Build-context dependencies that differ **per platform**, keyed by
    /// platform → the same `{group: [addr]}` shape as `context`.
    ///
    /// One buildx invocation builds every platform from one context, so a dep
    /// that differs by architecture cannot simply go in `context`: two variants
    /// of one target produce the same workspace-relative path and collide. Each
    /// platform's deps stage under their own prefix instead, and the Dockerfile
    /// selects with BuildKit's own `TARGETPLATFORM`:
    ///
    /// ```python
    /// platforms = ["linux/amd64", "linux/arm64"],
    /// context_by_platform = {
    ///     "linux/amd64": {"bin": ["//cmd/server:bin@v=linux_amd64"]},
    ///     "linux/arm64": {"bin": ["//cmd/server:bin@v=linux_arm64"]},
    /// },
    /// ```
    ///
    /// ```dockerfile
    /// ARG TARGETPLATFORM
    /// ARG CTX_BY_PLATFORM
    /// ARG SRC_BIN
    /// COPY ${CTX_BY_PLATFORM}/${TARGETPLATFORM}/${SRC_BIN} /usr/bin/server
    /// ```
    ///
    /// `SRC_<GROUP>` is relative to the platform prefix, so the group must
    /// produce the same paths on every platform — the usual case, one target
    /// built two ways. Groups here may not reuse a `context` group name, every
    /// platform in `platforms` must have an entry (and vice versa), and
    /// `platforms` must be explicit: the builder's default is not known until
    /// the build runs, so heph could not tell you which key to write.
    context_by_platform: HashMap<String, HashMap<String, Vec<String>>>,
    /// Base images, by build-context name → a single `oci_pull` (or `oci_image`)
    /// target address. Wired to `docker buildx --build-context <name>=…` so the
    /// Dockerfile can `FROM <name>` a base heph produced, instead of a registry
    /// ref BuildKit resolves on its own (which is neither hashed nor sandboxed).
    ///
    /// ```python
    /// bases = {"base": ":alpine"}      # Dockerfile: FROM base
    /// ```
    ///
    /// The name also works as a multi-stage source — `COPY --from=base /x /x`
    /// pulls files out of it without a `FROM`.
    ///
    /// The referenced target must expose an OCI **layout directory** — that is
    /// `oci_pull(layout = True)`. For a multi-platform build it must also be
    /// `all_platforms = True`: a layout holding one instance has no manifest for
    /// the other platforms, and the build fails on whichever one it was not
    /// pulled for. Hashed inputs, like `context`.
    bases: HashMap<String, Vec<String>>,
    /// Archive format: `oci` (default) or `docker`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
    /// `--build-arg` values passed to the build. Hashed (they change the image).
    /// A key may not contain `=` — docker would re-split it and silently take a
    /// different name/value pair than the one written here.
    build_args: HashMap<String, String>,
    /// Build a specific stage (`--target`) of a multi-stage Dockerfile, i.e. the
    /// name in `FROM … AS <stage>`. Named `stage` rather than buildx's `target`
    /// because a heph *target* is a different thing entirely.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    stage: Option<String>,
    /// Output archive filename, relative to the target's package. Must be a bare
    /// filename. Default `<target name>.tar`, so two image targets in one package
    /// do not declare the same output path.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    out: Option<String>,
    /// Target platforms (`--platform`), e.g. `["linux/amd64", "linux/arm64"]`.
    ///
    /// Left empty, the builder's own default platform is resolved at parse time
    /// (`docker buildx inspect --bootstrap`) and folded into the cache key —
    /// otherwise an arm64 laptop and an amd64 CI runner would compute the same
    /// key for different images and trade wrong-architecture artifacts through
    /// the remote cache.
    ///
    /// Multi-platform builds need a container-driver builder — see `builder`;
    /// the default daemon builder only builds one platform, and
    /// `format = "docker"` cannot hold a manifest list at all.
    platforms: Vec<String>,
    /// The buildx builder to build on (`docker buildx --builder`), e.g. a
    /// `docker buildx create --name multi --driver docker-container` instance.
    /// Left unset, buildx's own current builder is used.
    ///
    /// This is the attribute to reach for when a multi-platform build fails with
    /// "Multi-platform build is not supported for the docker driver": the
    /// default daemon builder cannot produce a manifest list, a
    /// `docker-container` one can.
    ///
    /// ```python
    /// oci_image(name = "img", builder = "multi",
    ///           platforms = ["linux/amd64", "linux/arm64"])
    /// ```
    ///
    /// It is a **hashed input**, and it is why `BUILDX_BUILDER` is stripped from
    /// the build's environment: which builder runs decides the platforms, the
    /// BuildKit version and the layer cache behind the image, so it has to be
    /// stated in the BUILD file where it lands in the cache key — not inherited
    /// from whichever shell happened to launch heph.
    ///
    /// What is hashed is the *name*. Two machines whose `multi` builders differ
    /// still agree on the key, the same way they do for the `docker` version
    /// itself; naming the builder narrows that gap, it does not close it.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    builder: Option<String>,
    /// BuildKit build secrets, as raw `--secret` specs, e.g.
    /// `["id=token,env=TOKEN"]`, consumed in the Dockerfile via
    /// `RUN --mount=type=secret`.
    ///
    /// The **spec string** is hashed; the secret's *value* is not, and cannot be
    /// (it would land in the `HEPH_DEBUG_HASH` trace). So a Dockerfile whose
    /// output depends on the secret's contents is not hermetic: rotating
    /// `TOKEN` does not invalidate the cache. Prefer secrets that gate *access*
    /// (a registry token) over ones that change the *result*.
    ///
    /// `src=` sources are rejected: the docker CLI resolves them against its own
    /// working directory, so the file would be read from outside the sandbox and
    /// would not be a declared input. Use `env=`, or stage the file through
    /// `context`.
    secrets: Vec<String>,
    /// SSH forwarding, as raw `--ssh` specs, e.g. `["default"]`, consumed via
    /// `RUN --mount=type=ssh`. Hashed as a string, with the same caveat as
    /// `secrets` — what the agent gives the build is not part of the key. `src=`
    /// sources are rejected for the same reason.
    ssh: Vec<String>,
    /// BuildKit `--cache-from` refs (registry or inline), e.g.
    /// `["type=registry,ref=reg/img:cache"]`. A build *optimization* — excluded
    /// from the input hash, so changing it never busts the heph cache.
    cache_from: Vec<String>,
    /// BuildKit `--cache-to` refs, e.g.
    /// `["type=registry,ref=reg/img:cache,mode=max"]` or `["type=inline"]`.
    /// A build optimization — excluded from the input hash.
    cache_to: Vec<String>,
    /// Caching for the built archive. Defaults to on for both the local and
    /// remote cache. `cache = False` disables both; the dict form
    /// `{enabled, remote, history}` toggles them independently.
    cache: TargetSpecCache,
}

/// Where the build's Dockerfile comes from.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DockerfileSource {
    /// A workspace-relative path (the spec's package-relative value joined onto
    /// the package), resolved against the sandbox workspace root. Some `context`
    /// dep has to put it there.
    Path(String),
    /// A target that produces it, staged as this target's own dep input under
    /// [`DOCKERFILE_ORIGIN`]. Its address is not hashed — the input's content
    /// hash is what the key follows, so two targets producing identical bytes
    /// are correctly one cache entry.
    Dep,
}

/// Origin id of the `dockerfile = ":target"` dep input.
const DOCKERFILE_ORIGIN: &str = "dockerfile";

/// Origin id of the builder-platform probe dep, present only when `platforms`
/// is empty.
const PLATFORM_ORIGIN: &str = "builder_platform";

/// The build-context subdirectory each platform's `context_by_platform` deps
/// stage under, exported to the Dockerfile as `CTX_BY_PLATFORM`. Inside the
/// context on purpose — unlike heph's own deps, the build has to read these.
const PLATFORM_CTX_PREFIX: &str = ".heph/ctx";

/// Build arg naming [`PLATFORM_CTX_PREFIX`], so a Dockerfile never hardcodes
/// heph's staging layout.
const PLATFORM_CTX_ARG: &str = "CTX_BY_PLATFORM";

/// A platform as a single path/id segment: `linux/arm64` → `linux_arm64`.
///
/// An origin id becomes a `input_<id>.list` filename, so it cannot carry the
/// `/` a platform string has.
fn platform_segment(platform: &str) -> String {
    platform.replace('/', "_")
}

/// Origin id for a per-platform context dep. The platform is *in* the id so a
/// `HEPH_DEBUG_HASH` trace names the arm64 binary rather than "a context dep".
fn platform_ctx_origin(platform: &str, group: &str, i: usize) -> String {
    format!(
        "context_by_platform|{}|{group}|{i}",
        platform_segment(platform)
    )
}

/// Stage each per-platform dep into the build context under its own platform
/// prefix, by hard link.
///
/// The deps unpack outside the context (one root per platform, so two variants
/// of one target do not collide on a shared path), but the build has to *read*
/// them — so they are linked into `<ws>/.heph/ctx/<platform>/…` here. Links, not
/// copies: the bytes are already staged, and a binary per platform per image is
/// exactly the thing not worth copying twice.
fn link_platform_contexts(
    req: &ManagedRunRequest<'_, '_>,
    def: &OciImageDef,
    context_dir: &Path,
) -> anyhow::Result<()> {
    for (platform, group) in &def.platform_context_keys {
        let seg = platform_segment(platform);
        let root = req.sandbox_dir.join(format!("exec_{seg}_{group}"));
        let dest_root = context_dir.join(PLATFORM_CTX_PREFIX).join(platform);
        for m in &req.inputs {
            let prefix = format!("context_by_platform|{seg}|{group}|");
            if !m.input.origin_id.starts_with(&prefix) {
                continue;
            }
            for path in dep_files(req, &m.input.origin_id)? {
                let rel = path.strip_prefix(&root).unwrap_or(&path);
                let dest = dest_root.join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create {parent:?}"))?;
                }
                // Fall back to a copy when the link cannot be made (a different
                // filesystem, or the sandbox already placed the file).
                if std::fs::hard_link(&path, &dest).is_err() && !dest.exists() {
                    std::fs::copy(&path, &dest)
                        .with_context(|| format!("stage {path:?} into the context at {dest:?}"))?;
                }
            }
        }
    }
    Ok(())
}

/// Unpack root for every dep that must **not** be part of the build context.
///
/// A dep with no `unpack_root` annotation materializes at the sandbox workspace
/// dir — which is exactly what `run` hands buildx as the build context. That is
/// right for `context` groups, whose whole purpose is to be in it, and wrong for
/// everything else: heph's own plumbing would ship inside the user's image on a
/// `COPY . /app`, and a `bases` layout (hundreds of MB) would be transferred to
/// BuildKit as context bytes *on top of* being read through `--build-context`.
///
/// Worse than either, silently: whether the probe file exists at all depends on
/// whether `platforms` is set, so adding `platforms = [...]` would change the
/// built image's contents with no error and nothing to point at.
///
/// These land in `<sandbox>/exec_<root>` instead. Both consumers read absolute
/// paths out of the dep's list file ([`dep_single_file`],
/// [`resolve_named_contexts`]), so where they land is immaterial to them.
fn out_of_context_root(origin: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("unpack_root".to_string(), format!("oci_{origin}"))])
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciImageDef {
    dockerfile: DockerfileSource,
    /// Workspace-relative output archive path (its basename is written into the
    /// sandbox package dir at run time).
    out: String,
    /// Workspace-relative digest output path.
    digest_out: String,
    format: ImageFormat,
    /// Sorted for a stable hash.
    build_args: BTreeMap<String, String>,
    stage: Option<String>,
    /// Platforms as written by the user. Empty means "whatever the builder
    /// defaults to", which arrives at run time from the [`platform`] probe dep —
    /// and reaches the cache key as that dep's content hash.
    platforms: Vec<String>,
    /// The buildx builder (`--builder`), as written by the user. `None` means
    /// buildx's current builder.
    builder: Option<String>,
    /// Sorted before hashing: `--secret a --secret b` and `--secret b --secret a`
    /// produce the same image, so they must produce the same key.
    secrets: Vec<String>,
    /// Sorted before hashing, as `secrets`.
    ssh: Vec<String>,
    /// The `(platform, group)` pairs `context_by_platform` declared, sorted.
    /// The deps' contents ride in via their own input hashes; this is here so
    /// that *adding or removing* a platform or a group busts the key, since the
    /// staged layout is what buildx sees. Sorted rather than order-sensitive:
    /// unlike `platforms`, these keys carry no manifest ordering.
    platform_context_keys: Vec<(String, String)>,
    /// Named build contexts (`--build-context <name>=…`) for base images, by
    /// group name. The origin_id is derived from the name; the resolved sandbox
    /// path is not known until run time, so only the names are hashed here —
    /// the contents arrive through the corresponding hashed dep inputs.
    bases: Vec<String>,
    /// Layer-cache sources — NOT hashed (build optimization only).
    cache_from: Vec<String>,
    /// Layer-cache destinations — NOT hashed.
    cache_to: Vec<String>,
}

/// Bump to invalidate cached builds when the output layout / arg recipe changes.
///
/// v2: build context rooted at the workspace dir (was the package dir), `SRC_*`
/// build args, `--build-context` bases, and the resolved builder platform in the
/// key. Every one of those changes what a given input set produces.
///
/// v3: the builder is selectable (`builder`) and hashed.
///
/// v4: platform strings are normalized before they reach the key, and
/// `context_by_platform` adds per-platform deps plus the `CTX_BY_PLATFORM` build
/// arg. Both change what a given input set produces.
const OCI_IMAGE_FORMAT_VERSION: u32 = 4;

impl Hash for OciImageDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_IMAGE_FORMAT_VERSION.hash(state);
        match &self.dockerfile {
            DockerfileSource::Path(p) => {
                "path".hash(state);
                p.hash(state);
            }
            // The address is deliberately absent: the dep's own hashout covers
            // what it produced, and hashing the address too would split the
            // cache on a rename that changes nothing.
            DockerfileSource::Dep => "dep".hash(state),
        }
        self.out.hash(state);
        self.digest_out.hash(state);
        self.format.output_type().hash(state);
        self.build_args.hash(state);
        self.stage.hash(state);
        // Order is significant here and only here: `--platform a,b` and
        // `--platform b,a` order the manifest list's entries differently.
        self.platforms.hash(state);
        self.builder.hash(state);
        self.secrets.hash(state);
        self.ssh.hash(state);
        self.bases.hash(state);
        self.platform_context_keys.hash(state);
        // `cache_from` / `cache_to` are deliberately excluded: they select where
        // BuildKit may reuse layers from, which changes how long a build takes
        // and can change the exported layer digests, but never what the image
        // does. Changing them must not bust an otherwise-valid heph entry — and
        // on a heph cache hit no build runs, so they have no effect at all.
    }
}

/// Uppercase a context group name into the `SRC_*` env/arg suffix, matching
/// `exec`'s convention so one Dockerfile idiom works across drivers.
fn arg_key_segment(group: &str) -> String {
    group
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The `SRC_*` build-arg name for a context group. The default (unnamed) group
/// is plain `SRC`.
fn src_arg_name(group: &str) -> String {
    if group.is_empty() {
        "SRC".to_string()
    } else {
        format!("SRC_{}", arg_key_segment(group))
    }
}

/// Assemble the `docker buildx build` argv. Pure so it can be unit-tested
/// without a docker daemon. `argv[0]` is the docker binary.
///
/// `src_args` and `named_contexts` are resolved at run time (they name paths
/// inside the sandbox), so they arrive as parameters rather than living on the
/// def.
#[expect(
    clippy::too_many_arguments,
    reason = "pure argv assembly: every parameter is a distinct part of the command line, and \
              bundling them into a struct would only move the same list one level away"
)]
fn build_argv(
    docker_bin: &str,
    def: &OciImageDef,
    context_dir: &Path,
    dockerfile_full: &Path,
    out_tar: &Path,
    metadata_file: &Path,
    named_contexts: &BTreeMap<String, String>,
    src_args: &BTreeMap<String, String>,
    // The builder's default platform, from the probe dep. `None` when
    // `platforms` is explicit and no probe was needed.
    probed_platform: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        docker_bin.to_string(),
        "buildx".to_string(),
        "build".to_string(),
        "--file".to_string(),
        dockerfile_full.to_string_lossy().into_owned(),
        "--output".to_string(),
        format!(
            "type={},dest={}",
            def.format.output_type(),
            out_tar.to_string_lossy()
        ),
        "--metadata-file".to_string(),
        metadata_file.to_string_lossy().into_owned(),
    ];

    if let Some(builder) = &def.builder {
        argv.push("--builder".to_string());
        argv.push(builder.clone());
    }
    if let Some(stage) = &def.stage {
        argv.push("--target".to_string());
        argv.push(stage.clone());
    }
    // An explicit `platforms` wins; otherwise the probe dep's answer, so the
    // argv states exactly what the key hashed rather than re-deriving it from
    // whatever the builder happens to default to now.
    let platform = if def.platforms.is_empty() {
        probed_platform.map(str::to_string)
    } else {
        Some(def.platforms.join(","))
    };
    if let Some(platform) = platform {
        argv.push("--platform".to_string());
        argv.push(platform);
    }
    // BTreeMaps iterate sorted → deterministic argv.
    for (name, path) in named_contexts {
        argv.push("--build-context".to_string());
        argv.push(format!("{name}={path}"));
    }
    for (k, v) in src_args {
        argv.push("--build-arg".to_string());
        argv.push(format!("{k}={v}"));
    }
    for (k, v) in &def.build_args {
        argv.push("--build-arg".to_string());
        argv.push(format!("{k}={v}"));
    }
    for secret in &def.secrets {
        argv.push("--secret".to_string());
        argv.push(secret.clone());
    }
    for ssh in &def.ssh {
        argv.push("--ssh".to_string());
        argv.push(ssh.clone());
    }
    for from in &def.cache_from {
        argv.push("--cache-from".to_string());
        argv.push(from.clone());
    }
    for to in &def.cache_to {
        argv.push("--cache-to".to_string());
        argv.push(to.clone());
    }
    argv.push(context_dir.to_string_lossy().into_owned());
    argv
}

/// Parse the image ref/id `docker load` printed to stdout, e.g.
/// `Loaded image: alpine:latest` or `Loaded image ID: sha256:abc…`. Takes the
/// last such line (a docker archive may load several).
pub(crate) fn parse_docker_load_ref(stdout: &str) -> anyhow::Result<String> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Loaded image ID:") {
            return Ok(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("Loaded image:") {
            return Ok(rest.trim().to_string());
        }
    }
    anyhow::bail!("no `Loaded image` line in docker load output: {stdout:?}")
}

/// Pin an image dep to the archive output group (`""`). An explicit group is
/// rejected rather than honoured: every other group on an `oci_image` target
/// (`digest`) is a text file, and handing skopeo a text file where it expects an
/// archive fails deep inside its layout parser.
pub(crate) fn pin_archive_group(
    image_ref: &mut TargetAddr,
    spec_value: &str,
) -> anyhow::Result<()> {
    match image_ref.output.as_deref() {
        None => {
            image_ref.output = Some(String::new());
            Ok(())
        }
        Some("") => Ok(()),
        Some(group) => anyhow::bail!(
            "`image` {spec_value:?} selects output group {group:?}; this driver consumes the \
             image archive, which is the default group. Drop the `|{group}` selector."
        ),
    }
}

/// Extract the image digest from a `docker buildx --metadata-file` JSON blob.
fn parse_metadata_digest(metadata: &str) -> anyhow::Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(metadata).context("parse buildx --metadata-file JSON")?;
    v.get("containerimage.digest")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .context("buildx metadata missing `containerimage.digest`")
}

/// Whether an attribute value names a target rather than a path.
///
/// The same two prefixes every other target-valued attribute here accepts
/// (`context`, `bases`, `image`): `//pkg:name` absolute, `:name` in this
/// package. No path worth writing starts with either.
pub(crate) fn is_addr(value: &str) -> bool {
    value.starts_with("//") || value.starts_with(':')
}

/// Join a package-relative path onto a (possibly empty) package prefix, yielding
/// a workspace-relative path.
pub(crate) fn ws_path(pkg: &str, rel: &str) -> String {
    if pkg.is_empty() {
        rel.to_string()
    } else {
        format!("{pkg}/{rel}")
    }
}

/// The platform a `skopeo`-driven pull/load resolves to when the BUILD file does
/// not name one: Linux on the host's own architecture.
///
/// The OS is pinned to `linux` rather than taken from the host because container
/// images are Linux images — on macOS, `skopeo` would otherwise ask a manifest
/// list for a `darwin` instance that does not exist and fail, while `docker`
/// would quietly get `linux` from the daemon's VM. Same target, two different
/// answers, one of them an error.
pub(crate) fn default_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("linux/{arch}")
}

/// Split `os/arch` into its parts.
/// Normalize a platform string to the spelling BuildKit and containerd use.
///
/// The same platform has several accepted spellings — `linux/x86_64` and
/// `linux/amd64` are the same machine, `linux/arm64/v8` and `linux/arm64` the
/// same CPU — and every one of them was previously carried verbatim into
/// `--platform`, into the cache key, and (once `context_by_platform` stages by
/// platform) into a directory name. Two spellings therefore meant two cache
/// entries for one image, and a staged directory a Dockerfile expanding
/// `TARGETPLATFORM` could never find.
///
/// This is a deliberate subset of containerd's rules — the aliases people
/// actually type, plus the redundant default variant — not a reimplementation
/// of its full table. An unknown os/arch passes through lowercased rather than
/// being rejected: BuildKit accepts platforms heph has never heard of, and
/// guessing wrong here would be worse than carrying the user's own word.
pub(crate) fn normalize_platform(platform: &str) -> anyhow::Result<String> {
    let lower = platform.to_ascii_lowercase();
    let (os, arch) = split_platform(&lower)?;
    let variant = lower.splitn(3, '/').nth(2).unwrap_or("");

    let arch = match arch {
        "x86_64" | "x86-64" => "amd64",
        "aarch64" => "arm64",
        "i386" | "i686" | "x86" => "386",
        other => other,
    };
    // containerd treats v8 as arm64's default and drops it; keeping it would
    // make `linux/arm64` and `linux/arm64/v8` two keys for one CPU.
    let variant = if arch == "arm64" && variant == "v8" {
        ""
    } else {
        variant
    };

    Ok(if variant.is_empty() {
        format!("{os}/{arch}")
    } else {
        format!("{os}/{arch}/{variant}")
    })
}

pub(crate) fn split_platform(platform: &str) -> anyhow::Result<(&str, &str)> {
    // A platform may carry a variant (`linux/arm/v7`); only os and arch are
    // addressable by skopeo's override flags.
    let mut parts = platform.splitn(3, '/');
    match (parts.next(), parts.next()) {
        (Some(os), Some(arch)) if !os.is_empty() && !arch.is_empty() => Ok((os, arch)),
        _ => anyhow::bail!("`platform` must look like `os/arch`, got {platform:?}"),
    }
}

/// The `skopeo` flags that pin which instance is taken out of a manifest list.
///
/// Without them skopeo matches against the host's own GOOS/GOARCH — which is
/// `darwin` on macOS, where no Linux image has a matching instance, so a
/// multi-arch archive fails to copy at all.
pub(crate) fn platform_override_args(platform: &str) -> anyhow::Result<[String; 4]> {
    let (os, arch) = split_platform(platform)?;
    Ok([
        "--override-os".to_string(),
        os.to_string(),
        "--override-arch".to_string(),
        arch.to_string(),
    ])
}

/// Extract the builder's first (default) platform from `docker buildx inspect`
/// output, whose relevant line reads:
///
/// ```text
/// Platforms:  linux/arm64, linux/amd64, linux/amd64/v2
/// ```
///
/// The first entry is the one a build with no `--platform` produces.
fn parse_builder_platform(inspect_out: &str) -> anyhow::Result<String> {
    for line in inspect_out.lines() {
        let Some(rest) = line.trim().strip_prefix("Platforms:") else {
            continue;
        };
        if let Some(first) = rest.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return Ok(first.to_string());
            }
        }
    }
    anyhow::bail!("no `Platforms:` line in `docker buildx inspect` output: {inspect_out:?}")
}

/// Reject a `secret`/`ssh` spec whose source is a host file path. The docker CLI
/// resolves `src=` against its own cwd, so the file is read from outside the
/// sandbox and never enters the input hash — a build that depends on it is not
/// reproducible on another machine.
fn ensure_no_src_source(kind: &str, specs: &[String]) -> anyhow::Result<()> {
    for spec in specs {
        let has_src = spec.split(',').any(|part| {
            part.trim_start().starts_with("src=") || part.trim_start().starts_with("source=")
        });
        if has_src {
            anyhow::bail!(
                "`{kind}` spec {spec:?} uses a `src=` host path, which is read from outside the \
                 sandbox and is not a hashed input. Use `env=`, or stage the file through \
                 `context` and reference it from the Dockerfile."
            );
        }
    }
    Ok(())
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

    /// Build a driver that shells out to `bin` instead of `docker`. Public so
    /// tests (including out-of-crate e2e) can point it at a fake.
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
        OciImageSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = OciImageSpec::from(&req.target_spec.config).context("parse oci_image config")?;
        let pkg = addr.package.clone();
        let pkg_str = addr.package.as_str();

        let format = ImageFormat::parse(spec.format.as_deref().unwrap_or("oci"))?;

        let dockerfile_spec = spec.dockerfile.unwrap_or_else(|| "Dockerfile".to_string());
        let dockerfile_ref = if is_addr(&dockerfile_spec) {
            Some(
                TargetAddr::parse(&dockerfile_spec, &pkg)
                    .with_context(|| format!("parse `dockerfile` target {dockerfile_spec:?}"))?,
            )
        } else {
            None
        };
        let dockerfile = if dockerfile_ref.is_some() {
            DockerfileSource::Dep
        } else {
            anyhow::ensure!(
                !Path::new(&dockerfile_spec).is_absolute(),
                "`dockerfile` {dockerfile_spec:?} is an absolute path: it would be read from \
                 outside the sandbox and would not be a declared input, so edits to it could never \
                 invalidate the cache. Use a path relative to the package, or the address of a \
                 target that produces it."
            );
            DockerfileSource::Path(ws_path(pkg_str, &dockerfile_spec))
        };

        let out_rel = spec.out.unwrap_or_else(|| format!("{}.tar", addr.name));
        anyhow::ensure!(
            Path::new(&out_rel).file_name().map(std::ffi::OsStr::new) == Some(out_rel.as_ref()),
            "`out` {out_rel:?} must be a bare filename (no directory component)"
        );
        let out = ws_path(pkg_str, &out_rel);
        let digest_out = ws_path(pkg_str, &format!("{}.digest", addr.name));

        for key in spec.build_args.keys() {
            anyhow::ensure!(
                !key.contains('='),
                "build arg name {key:?} contains `=`; docker would re-split it and take a \
                 different name/value pair than the one written here"
            );
        }
        ensure_no_src_source("secrets", &spec.secrets)?;
        ensure_no_src_source("ssh", &spec.ssh)?;

        let platforms = spec
            .platforms
            .iter()
            .map(|p| normalize_platform(p))
            .collect::<anyhow::Result<Vec<_>>>()
            .context("`platforms`")?;

        // Every failure below is a hard parse error naming the fix. The one
        // outcome that must never ship is silently dropping a platform's deps:
        // that removes a binary from an image with no signal at all.
        // Checked before the per-entry loop: with no `platforms` every entry is
        // "not in platforms", which is a true but useless thing to say.
        anyhow::ensure!(
            spec.context_by_platform.is_empty() || !platforms.is_empty(),
            "`context_by_platform` requires an explicit `platforms = [...]`: the builder's \
             default platform is not known until the build runs, so heph cannot tell which key \
             to write."
        );

        let mut pbp: Vec<(String, HashMap<String, Vec<String>>)> = Vec::new();
        for (raw, groups) in &spec.context_by_platform {
            let platform = normalize_platform(raw).context("`context_by_platform` key")?;
            anyhow::ensure!(
                platforms.contains(&platform),
                "`context_by_platform` declares {raw:?}, which is not in `platforms` \
                 ({platforms:?}). Add it to `platforms`, or drop the block."
            );
            for group in groups.keys() {
                anyhow::ensure!(
                    !spec.context.contains_key(group),
                    "group {group:?} is in both `context` and `context_by_platform`; they would \
                     export the same SRC_{} build arg. Rename one.",
                    arg_key_segment(group)
                );
            }
            pbp.push((platform, groups.clone()));
        }
        pbp.sort_by(|a, b| a.0.cmp(&b.0));

        if !pbp.is_empty() {
            for platform in &platforms {
                anyhow::ensure!(
                    pbp.iter().any(|(p, _)| p == platform),
                    "`platforms` lists {platform:?} but `context_by_platform` has no entry for \
                     it. Every platform must be covered, or the build fails on that leg alone \
                     with a missing COPY path. Add: {platform:?}: {{...}}"
                );
            }
        }

        if platforms.len() > 1 && format == ImageFormat::Docker {
            anyhow::bail!(
                "`platforms` lists {} platforms but `format = \"docker\"`; a docker-format archive \
                 holds a single image, not a manifest list. Use the default `format = \"oci\"`.",
                platforms.len()
            );
        }

        // With no explicit `platforms` the builder picks, so ask it which one and
        // put the answer in the key — otherwise two hosts with different default
        // platforms compute the same key for different images.
        let builder = spec.builder.filter(|b| !b.is_empty());

        // Build-context inputs: every file these targets produce lands in the
        // sandbox at its workspace-relative path, and the workspace root is the
        // build context — so a dep from any package is reachable.
        let mut inputs: Vec<Input> = Vec::new();

        // `dockerfile = ":target"` is a dep in its own right, so the user does
        // not have to also list it in `context` and spell the path it lands at.
        if let Some(r#ref) = dockerfile_ref {
            inputs.push(Input {
                r#ref,
                mode: InputMode::Standard,
                origin_id: DOCKERFILE_ORIGIN.to_string(),
                annotations: BTreeMap::new(),
                hashed: true,
                runtime: true,
            });
        }
        // With no explicit `platforms` the builder picks, and nothing else in
        // the key varies by platform — so depend on the probe target and let its
        // content hash carry the answer. An explicit `platforms` needs no probe
        // and declares no dep.
        if platforms.is_empty() {
            let probe = platform::addr_for(builder.as_deref());
            inputs.push(Input {
                r#ref: TargetAddr::parse(&probe, &pkg)
                    .with_context(|| format!("parse builder-platform probe addr {probe:?}"))?,
                mode: InputMode::Standard,
                origin_id: PLATFORM_ORIGIN.to_string(),
                annotations: out_of_context_root(PLATFORM_ORIGIN),
                hashed: true,
                runtime: true,
            });
        }

        // Per-platform context deps: staged under their own platform prefix, so
        // two variants of one target no longer collide on a shared path.
        let mut platform_context_keys: Vec<(String, String)> = Vec::new();
        for (platform, groups) in &pbp {
            let mut names: Vec<&String> = groups.keys().collect();
            names.sort();
            for group in names {
                platform_context_keys.push((platform.clone(), group.clone()));
                let refs = groups.get(group).map_or(&[][..], Vec::as_slice);
                for (i, r) in refs.iter().enumerate() {
                    let origin = platform_ctx_origin(platform, group, i);
                    inputs.push(Input {
                        r#ref: TargetAddr::parse(r, &pkg)?,
                        mode: InputMode::Standard,
                        // Inside the context, under the platform's own prefix.
                        // Outside the context: unpacking two variants of one
                        // target at their shared workspace-relative path would
                        // collide. `run` links them into the context under
                        // their platform prefix.
                        annotations: BTreeMap::from([(
                            "unpack_root".to_string(),
                            format!("{}_{group}", platform_segment(platform)),
                        )]),
                        origin_id: origin,
                        hashed: true,
                        runtime: true,
                    });
                }
            }
        }

        let mut context_groups: Vec<String> = spec.context.keys().cloned().collect();
        // HashMap iteration order varies per process; sort so the def and the
        // input ordering are byte-stable across runs.
        context_groups.sort();
        for group in &context_groups {
            let refs = spec.context.get(group).map_or(&[][..], Vec::as_slice);
            for (i, r) in refs.iter().enumerate() {
                inputs.push(Input {
                    r#ref: TargetAddr::parse(r, &pkg)?,
                    mode: InputMode::Standard,
                    origin_id: format!("context|{group}|{i}"),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                });
            }
        }

        // Base images: one dep each, consumed as a named `--build-context`.
        let mut bases: Vec<String> = spec.bases.keys().cloned().collect();
        bases.sort();
        for name in &bases {
            anyhow::ensure!(
                !name.is_empty(),
                "`bases` keys name a Dockerfile `FROM` target and cannot be empty"
            );
            let refs = spec.bases.get(name).map_or(&[][..], Vec::as_slice);
            let [image] = refs else {
                anyhow::bail!(
                    "`bases` entry {name:?} lists {} targets; a build context names exactly one \
                     image",
                    refs.len()
                );
            };
            inputs.push(Input {
                r#ref: TargetAddr::parse(image, &pkg)?,
                mode: InputMode::Standard,
                origin_id: format!("base|{name}"),
                annotations: out_of_context_root(&format!("base_{}", arg_key_segment(name))),
                hashed: true,
                runtime: true,
            });
        }

        let mut secrets = spec.secrets;
        secrets.sort();
        let mut ssh = spec.ssh;
        ssh.sort();

        let def = OciImageDef {
            dockerfile,
            out: out.clone(),
            digest_out: digest_out.clone(),
            format,
            build_args: spec.build_args.into_iter().collect(),
            stage: spec.stage,
            platforms,
            builder,
            secrets,
            ssh,
            bases,
            platform_context_keys,
            cache_from: spec.cache_from,
            cache_to: spec.cache_to,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("oci_image_{}", addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![
                    Output {
                        group: String::new(),
                        paths: vec![OutPath {
                            content: Content::FilePath(out),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }],
                    },
                    Output {
                        group: "digest".to_string(),
                        paths: vec![OutPath {
                            content: Content::FilePath(digest_out),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }],
                    },
                ],
                support_files: vec![],
                cache: spec.cache.into(),
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
        let def = req.request.target.def_de::<OciImageDef>().clone();
        let pkg_dir = req.sandbox_pkg_dir.clone();
        // The build context root is the sandbox *workspace* dir, not the package
        // dir: a `context` dep from another package materializes at its own
        // workspace-relative path, and a package-rooted context would not see
        // it. Dockerfile `COPY` paths are therefore workspace-relative.
        let context_dir = req.sandbox_ws_dir.clone();

        let dockerfile_full = match &def.dockerfile {
            // The dep staged it; its own list file says where.
            DockerfileSource::Dep => dep_single_file(&req, DOCKERFILE_ORIGIN)
                .context("oci_image: resolving the `dockerfile` target's output")?,
            DockerfileSource::Path(rel) => {
                let full = context_dir.join(rel);
                anyhow::ensure!(
                    full.exists(),
                    "oci_image: Dockerfile {rel:?} not found in the build context — declare the \
                     target that produces it in `context`, or set `dockerfile` to that target's \
                     address. Paths are workspace-relative (e.g. \"app/Dockerfile\")."
                );
                full
            }
        };

        // `parse` declared both of these in the package dir, and that is where
        // they have to end up for collection to find them.
        let out_name = basename(&def.out)?;
        let final_tar = pkg_dir.join(out_name);
        let digest_path = pkg_dir.join(basename(&def.digest_out)?);

        // The build writes the archive *outside* the workspace dir, then it is
        // moved into place below. The workspace dir is the build context, so
        // building straight into it would put the archive inside the context
        // that produces it: a wildcard `COPY` would pull the in-progress image
        // into the image, and BuildKit would be walking a tree the build is
        // still growing. Same reason the metadata file lands out here.
        let build_tar = req.sandbox_dir.join(out_name);
        let metadata_file = req.sandbox_dir.join("oci-metadata.json");

        // The probe dep's file holds the platform the key was computed from.
        let probed_platform = if def.platforms.is_empty() {
            let path = dep_single_file(&req, PLATFORM_ORIGIN)
                .context("oci_image: locating the builder-platform probe output")?;
            let raw = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("read builder platform {path:?}"))?;
            Some(raw.trim().to_string())
        } else {
            None
        };

        link_platform_contexts(&req, &def, &context_dir)
            .context("staging per-platform context deps")?;

        let named_contexts = resolve_named_contexts(&req, &def)?;
        let src_args = resolve_src_args(&req, &context_dir)?;
        let argv = build_argv(
            &self.docker_bin,
            &def,
            &context_dir,
            &dockerfile_full,
            &build_tar,
            &metadata_file,
            &named_contexts,
            &src_args,
            probed_platform.as_deref(),
        );

        let addr = req.request.target.addr.format();
        let mut io = ToolIo::from_request(&mut req.request);
        run_tool(argv, &context_dir, "docker buildx build", &mut io, ctoken)
            .await
            .map_err(|e| build_error_hint(e, &def, &addr))?;

        let metadata = tokio::fs::read_to_string(&metadata_file)
            .await
            .with_context(|| format!("read buildx metadata {metadata_file:?}"))?;
        let digest = parse_metadata_digest(&metadata)?;

        // Same filesystem (both live under the sandbox), so this is a rename,
        // not a copy of the whole image.
        tokio::fs::rename(&build_tar, &final_tar)
            .await
            .with_context(|| format!("move the built archive {build_tar:?} to {final_tar:?}"))?;
        tokio::fs::write(&digest_path, &digest)
            .await
            .with_context(|| format!("write digest {digest_path:?}"))?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Turn a `docker buildx build` failure into one the user can act on.
///
/// BuildKit's own message is kept — it is usually the specific one — and the
/// heph-side remedy is layered on top for the two failures whose fix lives in
/// the BUILD file rather than in the Dockerfile.
fn build_error_hint(e: anyhow::Error, def: &OciImageDef, addr: &str) -> anyhow::Error {
    let text = format!("{e:#}");

    // The `docker` driver (a plain daemon builder without the containerd image
    // store) has no file exporters at all: it can only load or push into the
    // daemon. `oci_image` always writes an archive, so on such a builder *every*
    // build fails, whatever `format` says — and BuildKit's message names the
    // exporter without saying how to get one.
    if text.contains("exporter is not supported for the docker driver") {
        let fix = match &def.builder {
            Some(b) => format!(
                "builder {b:?} uses the `docker` driver, which cannot write an image archive to a \
                 file. Recreate it with `docker buildx create --name {b} --driver docker-container`"
            ),
            None => "the current buildx builder uses the `docker` driver, which cannot write an \
                     image archive to a file. Create one that can — `docker buildx create --name \
                     heph --driver docker-container` — and select it with `builder = \"heph\"`"
                .to_string(),
        };
        return e.context(format!(
            "oci_image {addr}: {fix} (or turn on the daemon's containerd image store, which gives \
             the `docker` driver the same exporters)"
        ));
    }

    if def.platforms.len() <= 1 {
        return e.context(format!("oci_image {addr}"));
    }
    // The two ways a multi-platform build fails that a single-platform one
    // cannot: the builder does not do them at all, and a single-instance base
    // has no manifest for the other platforms. Both read as an opaque BuildKit
    // error otherwise.
    let bases = if def.bases.is_empty() {
        String::new()
    } else {
        format!(
            ", and every base in `bases` ({}) must be pulled with `all_platforms = True`",
            def.bases.join(", ")
        )
    };
    let builder = def.builder.as_deref().map_or_else(
        || {
            "multi-platform builds need a container builder — create one with `docker buildx \
             create --name multi --driver docker-container` and select it with `builder = \
             \"multi\"`"
                .to_string()
        },
        |b| {
            format!(
                "multi-platform builds need a container builder — check that {b:?} is one \
                 (`docker buildx inspect {b}` should say `Driver: docker-container`)"
            )
        },
    );
    e.context(format!("oci_image {addr}: {builder}{bases}"))
}

/// Host env vars forwarded to `docker` / `skopeo`. Everything else is cleared
/// (`proc_exec::Spec` populates a cleared environment from this list), so a
/// stray `BUILDX_BUILDER` or `SOURCE_DATE_EPOCH` cannot change the image behind
/// the cache key's back.
///
/// What is passed and why:
/// - `PATH` — resolve the `docker`/`skopeo` binary and the credential helpers
///   the CLI execs (`docker-credential-*`).
/// - `HOME`, `DOCKER_CONFIG`, `REGISTRY_AUTH_FILE`, `XDG_RUNTIME_DIR` — locate
///   the registry credentials. Auth decides whether a pull/push is permitted,
///   never what the resulting bytes are, so it is deliberately not hashed.
/// - `DOCKER_HOST`, `DOCKER_CONTEXT` — which daemon to talk to. Hashed via
///   [`OciImageDef::builder_platform`] instead of directly: what matters for the
///   artifact is the platform the selected builder produces, not its address.
/// - TLS/proxy vars — needed on hosts behind a MITM proxy or with a non-default
///   CA bundle, exactly as `plugin-nix` passes them.
///
/// Deliberately NOT passed: `BUILDX_BUILDER`, `BUILDKIT_HOST`,
/// `DOCKER_DEFAULT_PLATFORM`, `SOURCE_DATE_EPOCH`, `DOCKER_BUILDKIT` — each
/// changes the output while the def hash stays put. A user who needs one of
/// these wants a spec field, not an ambient variable: `BUILDX_BUILDER` has one
/// (`builder`), which is hashed, and `DOCKER_DEFAULT_PLATFORM` is what
/// `platforms` is for.
fn passthrough_oci_env() -> Vec<(OsString, OsString)> {
    let mut out = Vec::new();
    for name in &[
        "PATH",
        "HOME",
        "DOCKER_CONFIG",
        "REGISTRY_AUTH_FILE",
        "XDG_RUNTIME_DIR",
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
    ] {
        if let Ok(v) = std::env::var(name) {
            out.push((OsString::from(name), OsString::from(v)));
        }
    }
    out
}

/// How much of a failed tool's stderr is quoted in the error. The full stream
/// was already relayed to the user's terminal; the error only needs enough tail
/// to name the cause without pasting a multi-megabyte BuildKit log into the
/// failure registry.
const ERR_TAIL_BYTES: usize = 8 * 1024;

/// Ring buffer holding the last [`ERR_TAIL_BYTES`] bytes of a stream. Bounded so
/// a chatty build cannot grow the driver's memory with a log nobody will read.
#[derive(Default)]
struct TailBuf {
    buf: Vec<u8>,
    truncated: bool,
}

impl TailBuf {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > ERR_TAIL_BYTES {
            let drop_to = self.buf.len() - ERR_TAIL_BYTES;
            self.buf.drain(..drop_to);
            self.truncated = true;
        }
    }

    fn render(&self) -> String {
        let text = String::from_utf8_lossy(&self.buf);
        if self.truncated {
            format!("…(earlier output truncated)…{text}")
        } else {
            text.into_owned()
        }
    }
}

/// Relay the child's output to the user's terminal as it arrives, while keeping
/// a bounded tail of each stream for the error message and accumulating stdout.
///
/// Both streams arrive interleaved through a single [`proc_exec::OutputReader`]
/// tagged by [`proc_exec::StreamId`] — one reader, not two, is what keeps a
/// chatty stderr from head-of-line blocking stdout on macOS.
///
/// Only stdout is accumulated: for every command here it is a short status line
/// (`Loaded image: …`), while BuildKit's progress goes to stderr and would be
/// megabytes.
async fn tee<'w>(
    reader: Option<proc_exec::OutputReader>,
    // The trait objects' lifetime is a free parameter: `&mut dyn Trait` is
    // invariant, so it cannot be shortened to the borrow's at the call site.
    mut stdout_sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin + 'w)>,
    mut stderr_sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin + 'w)>,
    log: Option<&std::sync::Mutex<std::fs::File>>,
    out_tail: &std::sync::Mutex<TailBuf>,
    err_tail: &std::sync::Mutex<TailBuf>,
    captured: &std::sync::Mutex<Vec<u8>>,
) {
    use std::io::Write as _;
    use tokio::io::AsyncWriteExt as _;
    let Some(mut reader) = reader else { return };
    while let Ok(Some((stream, chunk))) = reader.recv().await {
        // Both streams land in the target's log, in arrival order — the same
        // shape `pluginexec` gives a `bash` target's output.
        if let Some(log) = log
            && let Ok(mut f) = log.lock()
        {
            drop(f.write_all(&chunk));
        }
        let (tail, sink) = match stream {
            proc_exec::StreamId::Stdout => {
                if let Ok(mut c) = captured.lock() {
                    c.extend_from_slice(&chunk);
                }
                (out_tail, &mut stdout_sink)
            }
            proc_exec::StreamId::Stderr => (err_tail, &mut stderr_sink),
        };
        if let Ok(mut t) = tail.lock() {
            t.push(&chunk);
        }
        if let Some(out) = sink {
            drop(out.write_all(&chunk).await);
            drop(out.flush().await);
        }
    }
}

/// Spawn a host tool, relay its output to the user, and wait for it — killing
/// the child if the request is cancelled.
///
/// This goes through `hproc::proc_exec` rather than `std::process::Command` for
/// four reasons, all of which a container build makes acute: the child is
/// `SIGKILL`ed on cancel (a detached `docker buildx` would otherwise keep
/// streaming into a sandbox the cleaner is deleting, and park runtime shutdown
/// until it finished), the environment is cleared and explicitly repopulated,
/// the working directory is explicit, and the wait does not ride the
/// `spawn_blocking` waker that `hcore::blocking` documents as lossy on macOS.
///
/// Returns captured stdout.
pub(crate) async fn run_tool(
    argv: Vec<String>,
    cwd: &Path,
    what: &'static str,
    io: &mut ToolIo<'_>,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<String> {
    if ctoken.is_cancelled() {
        return Err(anyhow::Error::new(hplugin::error::CancelledError)).context(what);
    }
    let (bin, args) = argv.split_first().context("empty argv (internal bug)")?;

    let spec = proc_exec::Spec {
        program: PathBuf::from(bin),
        args: args.iter().map(OsString::from).collect(),
        env: passthrough_oci_env(),
        cwd: cwd.to_path_buf(),
        stdin: proc_exec::StdioSpec::Null,
        stdout: proc_exec::StdioSpec::Piped,
        stderr: proc_exec::StdioSpec::Piped,
        // setsid: buildx forks helper processes (and the docker CLI forks
        // credential helpers); the supervisor's killpg needs the whole group.
        setsid: true,
        ctty: false,
    };

    let mut handle = proc_exec::spawn(spec).map_err(|e| missing_tool_error(e, bin, what))?;
    let output_reader = handle.take_output();

    let out_tail = std::sync::Mutex::new(TailBuf::default());
    let err_tail = std::sync::Mutex::new(TailBuf::default());
    let captured = std::sync::Mutex::new(Vec::new());

    let (stdout_sink, stderr_sink, log) = io.sinks();
    let io_fut = tee(
        output_reader,
        stdout_sink,
        stderr_sink,
        log,
        &out_tail,
        &err_tail,
        &captured,
    );

    // The wait must not share a task with the reader: it parks its worker until
    // the child exits, so the pipes would stop draining and a build that fills
    // one would wedge forever. `spawn_wait` is the only public wait for exactly
    // that reason, and it carries the cancellation escalation
    // (SIGINT → grace → SIGKILL) inside the spawned task.
    let wait_handle = handle.spawn_wait(ctoken.clone_arc());
    let (wait_res, ()) = tokio::join!(wait_handle, io_fut);

    let status = match wait_res.context("wait task for the child")? {
        Ok(s) => s,
        Err(e) if ctoken.is_cancelled() => {
            return Err(anyhow::Error::new(hplugin::error::CancelledError))
                .with_context(|| format!("{what}: {e}"));
        }
        Err(e) => return Err(anyhow::Error::new(e)).with_context(|| format!("wait for {what}")),
    };

    if !status.success() {
        let tail = err_tail
            .lock()
            .map(|t| t.render())
            .unwrap_or_else(|_| String::new());
        let tail = if tail.trim().is_empty() {
            out_tail
                .lock()
                .map(|t| t.render())
                .unwrap_or_else(|_| String::new())
        } else {
            tail
        };
        anyhow::bail!("{what} failed ({status}): {}", tail.trim_end());
    }

    let captured = captured.into_inner().unwrap_or_default();
    Ok(String::from_utf8_lossy(&captured).into_owned())
}

/// Turn the bare `No such file or directory` a missing tool produces into
/// something the reader can act on. `docker` and `skopeo` are host capabilities
/// heph does not install, so "not found" is a routine first-run state, not an
/// internal error.
fn missing_tool_error(e: std::io::Error, bin: &str, what: &str) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        let hint = match Path::new(bin).file_name().and_then(|n| n.to_str()) {
            Some("skopeo") => {
                " — install skopeo, or set `tool = \"docker\"` on a `format = \"docker\"` target"
            }
            Some("docker") => " — install Docker (the `oci_image` driver needs `docker buildx`)",
            _ => "",
        };
        anyhow::anyhow!("{what}: `{bin}` not found on PATH{hint}")
    } else {
        anyhow::Error::new(e).context(format!("{what}: spawn `{bin}`"))
    }
}

/// All paths a Dep input materialized into the sandbox, read from its `.list`
/// file (one absolute path per line — see `driver_managed.rs::list_path_for`).
fn dep_files(req: &ManagedRunRequest<'_, '_>, origin_id: &str) -> anyhow::Result<Vec<PathBuf>> {
    let Some(m) = req.inputs.iter().find(|m| {
        m.input.origin_id == origin_id
            && matches!(
                m.input.artifact.r#type,
                hplugin::driver::inputartifact::Type::Dep
            )
    }) else {
        return Ok(Vec::new());
    };
    let list_path = m.require_list_path()?;
    let content = std::fs::read_to_string(list_path)
        .with_context(|| format!("read dep list {list_path:?}"))?;
    Ok(content
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Build the `SRC_*` build args: for each context group, the paths that group
/// produced, expressed relative to the build context root so a Dockerfile can
/// `COPY ${SRC_FOO} …` directly.
///
/// Multiple paths in one group are space-separated, matching what `COPY` accepts
/// and what `exec`'s `SRC_*` env vars already do.
fn resolve_src_args(
    req: &ManagedRunRequest<'_, '_>,
    context_dir: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Per-platform groups: paths are relative to the platform prefix, not to the
    // context, so one `SRC_<GROUP>` works for every platform and the Dockerfile
    // joins it with `TARGETPLATFORM`. That requires the group to produce the
    // same paths everywhere — one target built two ways, the usual case — so
    // disagreement is an error rather than a silently platform-dependent arg.
    let mut by_platform_group: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for m in &req.inputs {
        if let Some(rest) = m.input.origin_id.strip_prefix("context_by_platform|") {
            // `context_by_platform|<platform>|<group>|<index>`; the platform
            // itself contains `/` but never `|`.
            let mut parts = rest.splitn(2, '|');
            let (Some(platform), Some(tail)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some((group, _)) = tail.rsplit_once('|') else {
                continue;
            };
            let root = req.sandbox_dir.join(format!("exec_{platform}_{group}"));
            for path in dep_files(req, &m.input.origin_id)? {
                let rel = path.strip_prefix(&root).unwrap_or(&path);
                by_platform_group
                    .entry(group.to_string())
                    .or_default()
                    .entry(platform.to_string())
                    .or_default()
                    .push(rel.to_string_lossy().into_owned());
            }
            continue;
        }
        let Some(rest) = m.input.origin_id.strip_prefix("context|") else {
            continue;
        };
        // origin_id is `context|<group>|<index>`; the group may itself be empty.
        let Some((group, _)) = rest.rsplit_once('|') else {
            continue;
        };
        for path in dep_files(req, &m.input.origin_id)? {
            let rel = path.strip_prefix(context_dir).unwrap_or(&path);
            by_group
                .entry(group.to_string())
                .or_default()
                .push(rel.to_string_lossy().into_owned());
        }
    }
    let mut args: BTreeMap<String, String> = by_group
        .into_iter()
        .map(|(group, mut paths)| {
            // Stable order: the arg feeds the build and therefore the image.
            paths.sort();
            (src_arg_name(&group), paths.join(" "))
        })
        .collect();

    let has_platform_groups = !by_platform_group.is_empty();
    for (group, per_platform) in by_platform_group {
        let mut agreed: Option<(String, String)> = None;
        for (platform, mut paths) in per_platform {
            paths.sort();
            let joined = paths.join(" ");
            match &agreed {
                None => agreed = Some((platform, joined)),
                Some((first_platform, first)) if *first != joined => anyhow::bail!(
                    "`context_by_platform` group {group:?} produces {first:?} for \
                     {first_platform} but {joined:?} for {platform}. SRC_{} names one path set \
                     for every platform — split into separate groups, or COPY the literal path.",
                    arg_key_segment(&group)
                ),
                Some(_) => {}
            }
        }
        if let Some((_, joined)) = agreed {
            args.insert(src_arg_name(&group), joined);
        }
    }
    // Only when there is something to select: a target with no per-platform
    // groups should not carry a build arg naming a directory it never staged.
    if has_platform_groups {
        args.insert(
            PLATFORM_CTX_ARG.to_string(),
            PLATFORM_CTX_PREFIX.to_string(),
        );
    }
    Ok(args)
}

/// Resolve each `bases` entry to a `--build-context <name>=oci-layout://<dir>`
/// value. The dep must have materialized an OCI layout *directory* (what
/// `oci_pull(layout = True)` produces) — buildx's `oci-layout://` reads a layout
/// tree, not a tar archive.
fn resolve_named_contexts(
    req: &ManagedRunRequest<'_, '_>,
    def: &OciImageDef,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for name in &def.bases {
        let origin = format!("base|{name}");
        let paths = dep_files(req, &origin)?;
        anyhow::ensure!(
            !paths.is_empty(),
            "`bases` entry {name:?} produced no files in the sandbox"
        );
        // A dep's unpack list names *files*, never the directories holding them,
        // so the layout root cannot be found by looking for a directory entry.
        // Locate it by its marker instead: an OCI layout is exactly a tree with
        // an `oci-layout` file at its root, which is also the thing buildx's
        // `oci-layout://` wants pointed at.
        let dir = paths
            .iter()
            .find(|p| p.file_name().is_some_and(|n| n == "oci-layout"))
            .and_then(|p| p.parent())
            .with_context(|| {
                format!(
                    "`bases` entry {name:?} is not an OCI layout directory: no `oci-layout` file \
                     among {} staged path(s), the first being {:?}. Build the base with \
                     `oci_pull(layout = True)` — a plain archive cannot be a build context.",
                    paths.len(),
                    paths.first()
                )
            })?;
        out.insert(
            name.clone(),
            format!("oci-layout://{}", dir.to_string_lossy()),
        );
    }
    Ok(out)
}

/// Absolute path to the single file a Dep input materialized into the sandbox.
/// Reads the input's `.list` file (one absolute path per line — see
/// `driver_managed.rs::list_path_for`). Errors unless exactly one file was
/// produced, so a caller expecting one archive fails loudly on a mis-declared
/// dep.
pub(crate) fn dep_single_file(
    req: &ManagedRunRequest<'_, '_>,
    origin_id: &str,
) -> anyhow::Result<PathBuf> {
    let m = req
        .inputs
        .iter()
        .find(|m| {
            m.input.origin_id == origin_id
                && matches!(
                    m.input.artifact.r#type,
                    hplugin::driver::inputartifact::Type::Dep
                )
        })
        .with_context(|| format!("no dep input {origin_id:?} in sandbox"))?;
    let list_path = m.require_list_path()?;
    // The `.list` file holds one absolute materialized path per line.
    let content = std::fs::read_to_string(list_path)
        .with_context(|| format!("read dep list {list_path:?}"))?;
    let mut paths = content.lines().filter(|l| !l.is_empty());
    let first = paths
        .next()
        .with_context(|| format!("dep {origin_id:?} produced no files"))?;
    anyhow::ensure!(
        paths.next().is_none(),
        "dep {origin_id:?} produced more than one file; expected exactly one archive"
    );
    Ok(PathBuf::from(first))
}

fn basename(path: &str) -> anyhow::Result<&std::ffi::OsStr> {
    Path::new(path)
        .file_name()
        .with_context(|| format!("path {path:?} has no file name"))
}

/// Fake-binary scaffolding shared by every driver's `run()` tests.
///
/// The drivers' whole job is to assemble a command and interpret its result, so
/// the interesting behaviour only shows up once something is actually executed.
/// These helpers stand a shell script in for `docker` / `skopeo`, record what it
/// was called with, and let a test dictate how it behaves.
#[cfg(test)]
pub(crate) mod testfake {
    use hdriver_support::driver_managed::{ManagedRunInput, ManagedRunRequest};
    use hplugin::driver::targetdef::TargetDef;
    use hplugin::driver::{RunInput, RunRequest, inputartifact, outputartifact};
    use std::path::PathBuf;

    /// A sandbox laid out the way the managed-driver bridge lays one out.
    pub(crate) struct Sandbox {
        pub dir: tempfile::TempDir,
        pub ws: PathBuf,
        pub pkg: PathBuf,
        /// Where fake binaries append one line per invocation.
        pub log: PathBuf,
    }

    impl Sandbox {
        pub(crate) fn new(package: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let ws = dir.path().join("ws");
            let pkg = if package.is_empty() {
                ws.clone()
            } else {
                ws.join(package)
            };
            std::fs::create_dir_all(&pkg).expect("mkdir pkg");
            let log = dir.path().join("calls.log");
            Sandbox { dir, ws, pkg, log }
        }

        /// Install an executable fake at `name`. `body` runs after the call has
        /// been recorded; `$@` is the argv and `$LOG` the record file.
        ///
        /// The log path is derived from `$0` rather than an environment
        /// variable, because the driver clears the child's environment — which
        /// is the point, and which a fake depending on an inherited var would
        /// quietly defeat.
        pub(crate) fn fake(&self, name: &str, body: &str) -> String {
            let path = self.dir.path().join(name);
            let script = format!(
                "#!/bin/sh\nLOG=\"$(dirname \"$0\")/calls.log\"\nprintf '%s' \"{name}\" >> \
                 \"$LOG\"\nfor a in \"$@\"; do printf ' %s' \"$a\" >> \"$LOG\"; done\nprintf \
                 '\\n' >> \"$LOG\"\n{body}\n"
            );
            // Not `fs::write` + `set_permissions`: tests run in parallel, and a
            // sibling test's fork between our create and our exec inherits a
            // writable fd to this file, so the exec fails with `ETXTBSY`.
            // `write_executable` drains those descriptors before returning.
            hcore::fsutil::write_executable(&path, script.as_bytes()).expect("write fake");
            path.to_string_lossy().into_owned()
        }

        /// One entry per invocation, in call order.
        pub(crate) fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    /// Build a `ManagedRunRequest` over `sandbox` for `def`, with one Dep input
    /// per (origin_id, materialized paths) pair.
    /// What the seeded probe dep reports. Tests asserting on `--platform` match
    /// this; a test that needs another platform passes its own probe dep.
    pub(crate) const PROBED_PLATFORM: &str = "linux/arm64";

    pub(crate) fn run_request<'a>(
        request_id: &'a String,
        hashin: &'a str,
        def: &'a TargetDef,
        sandbox: &Sandbox,
        deps: &[(&str, Vec<PathBuf>)],
    ) -> ManagedRunRequest<'a, 'static> {
        let list_dir = sandbox.dir.path().join("lists");
        std::fs::create_dir_all(&list_dir).expect("mkdir lists");

        // Every `oci_image` without explicit `platforms` depends on the probe
        // target, so a runnable request needs its output. Seeded here rather
        // than in each test: it is a precondition of running at all, not
        // something an individual test is making a statement about. A test that
        // wants a specific platform passes its own.
        let mut deps: Vec<(&str, Vec<PathBuf>)> = deps.to_vec();
        if !deps.iter().any(|(id, _)| *id == super::PLATFORM_ORIGIN) {
            let probe = sandbox.dir.path().join("probed-platform.txt");
            std::fs::write(&probe, PROBED_PLATFORM).expect("write probe output");
            deps.push((super::PLATFORM_ORIGIN, vec![probe]));
        }

        let mut inputs = Vec::new();
        for (origin_id, paths) in &deps {
            let list_path = list_dir.join(format!("input_{origin_id}.list"));
            let body: String = paths
                .iter()
                .map(|p| format!("{}\n", p.to_string_lossy()))
                .collect();
            std::fs::write(&list_path, body).expect("write list");
            inputs.push(ManagedRunInput {
                input: RunInput {
                    artifact: inputartifact::InputArtifact {
                        r#type: inputartifact::Type::Dep,
                        origin_id: (*origin_id).to_string(),
                        content: std::sync::Arc::new(outputartifact::OutputArtifact {
                            group: String::new(),
                            name: String::new(),
                            r#type: outputartifact::Type::Output,
                            content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                                data: vec![],
                                path: String::new(),
                                x: false,
                            }),
                            hashout: "test".to_string(),
                        }),
                    },
                    origin_id: (*origin_id).to_string(),
                    source_addr: hmodel::htaddr::parse_addr("//test:dep").expect("addr"),
                    filters: vec![],
                    annotations: std::collections::BTreeMap::new(),
                },
                list_path: Some(list_path),
                unpack_root: sandbox.ws.clone(),
            });
        }

        ManagedRunRequest {
            request: RunRequest {
                request_id,
                target: def,
                tree_root_path: sandbox.ws.clone(),
                inputs: vec![],
                hashin,
                stdin: None,
                stdout: None,
                stderr: None,
                sandbox_dir: sandbox.dir.path().to_path_buf(),
            },
            sandbox_dir: sandbox.dir.path().to_path_buf(),
            sandbox_ws_dir: sandbox.ws.clone(),
            sandbox_pkg_dir: sandbox.pkg.clone(),
            inputs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testfake::{Sandbox, run_request};
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;

    /// A fake `docker` that answers the builder probe and, on `buildx build`,
    /// writes the metadata file and the output archive the driver expects.
    const FAKE_DOCKER_OK: &str = r#"
case "$2" in
  inspect) echo "Name: default"; echo "Platforms: linux/arm64, linux/amd64" ;;
  build)
    meta=""; dest=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --metadata-file) meta="$2" ;;
        --output) dest="${2#*dest=}" ;;
      esac
      shift
    done
    [ -n "$meta" ] && printf '{"containerimage.digest":"sha256:deadbeef"}' > "$meta"
    [ -n "$dest" ] && printf 'tar-bytes' > "$dest"
    ;;
esac
exit 0
"#;

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

    /// A context dep, so `parse` sees the common shape.
    fn ctx() -> (&'static str, Value) {
        (
            "context",
            Value::List(vec![Value::String(":srcs".to_string())]),
        )
    }

    /// `parse` on a driver wired to a fake docker — the builder probe must not
    /// need a real daemon.
    async fn parse_in(
        sandbox: &Sandbox,
        addr: &str,
        config: HashMap<String, Value>,
    ) -> ParseResponse {
        let bin = sandbox.fake("docker", FAKE_DOCKER_OK);
        Driver::with_binary(bin)
            .parse(parse_req(addr, config), &StdCancellationToken::new())
            .await
            .expect("parse")
    }

    async fn parse_err(sandbox: &Sandbox, addr: &str, config: HashMap<String, Value>) -> String {
        let bin = sandbox.fake("docker", FAKE_DOCKER_OK);
        let err = Driver::with_binary(bin)
            .parse(parse_req(addr, config), &StdCancellationToken::new())
            .await
            .err()
            .expect("parse must fail");
        format!("{err:#}")
    }

    #[test]
    fn format_parse_rejects_unknown() {
        assert_eq!(ImageFormat::parse("oci").expect("oci"), ImageFormat::Oci);
        assert_eq!(
            ImageFormat::parse("docker").expect("docker"),
            ImageFormat::Docker
        );
        let err = ImageFormat::parse("tar").expect_err("unknown format");
        assert!(format!("{err:#}").contains("oci"), "got: {err:#}");
    }

    #[test]
    fn builder_platform_takes_the_first_listed() {
        let out = "Name: default\nDriver: docker\nPlatforms: linux/arm64, linux/amd64\n";
        assert_eq!(parse_builder_platform(out).expect("parse"), "linux/arm64");
        let err = parse_builder_platform("Name: default\n").expect_err("no platforms line");
        assert!(format!("{err:#}").contains("Platforms"), "got: {err:#}");
    }

    /// The same machine has several accepted spellings. Carrying them verbatim
    /// meant two cache entries for one image — and, once deps stage by platform,
    /// a directory name a Dockerfile expanding `TARGETPLATFORM` could not find.
    #[test]
    fn platform_spellings_normalize_to_one() {
        for (raw, want) in [
            ("linux/x86_64", "linux/amd64"),
            ("linux/x86-64", "linux/amd64"),
            ("linux/AMD64", "linux/amd64"),
            ("linux/aarch64", "linux/arm64"),
            // containerd treats v8 as arm64's default and drops it.
            ("linux/arm64/v8", "linux/arm64"),
            ("linux/i386", "linux/386"),
            // A real variant is not a default and must survive.
            ("linux/arm/v7", "linux/arm/v7"),
            ("linux/amd64", "linux/amd64"),
            // Unknown but well-formed: heph carries the user's own word rather
            // than guessing, since BuildKit accepts platforms we have not heard of.
            ("linux/riscv64", "linux/riscv64"),
            ("windows/amd64", "windows/amd64"),
        ] {
            assert_eq!(normalize_platform(raw).expect("normalize"), want, "{raw}");
        }
        assert!(
            normalize_platform("linux").is_err(),
            "os alone is not a platform"
        );
    }

    /// Two spellings of one platform must be one cache entry, not two.
    #[tokio::test]
    async fn platform_spelling_does_not_split_the_cache() {
        let sbx = Sandbox::new("app");
        let with = |p: &str| {
            cfg(&[
                ctx(),
                ("platforms", Value::List(vec![Value::String(p.to_string())])),
            ])
        };
        let a = parse_in(&sbx, "//app:img", with("linux/amd64")).await;
        let b = parse_in(&sbx, "//app:img", with("linux/x86_64")).await;
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "linux/x86_64 and linux/amd64 are the same platform"
        );
    }

    #[test]
    fn src_arg_names_follow_the_exec_convention() {
        assert_eq!(src_arg_name(""), "SRC");
        assert_eq!(src_arg_name("bin"), "SRC_BIN");
        assert_eq!(src_arg_name("my-group"), "SRC_MY_GROUP");
    }

    /// The build command carries the format, dockerfile, output dest, metadata
    /// file, sorted build args and cache refs, ending in the context dir.
    #[test]
    fn build_argv_assembles_expected_command() {
        let def = OciImageDef {
            dockerfile: DockerfileSource::Path("app/Dockerfile".to_string()),
            out: "app/img.tar".to_string(),
            digest_out: "app/img.digest".to_string(),
            format: ImageFormat::Oci,
            build_args: BTreeMap::from([
                ("B".to_string(), "2".to_string()),
                ("A".to_string(), "1".to_string()),
            ]),
            stage: Some("runtime".to_string()),
            platforms: vec!["linux/amd64".to_string(), "linux/arm64".to_string()],
            builder: None,
            secrets: vec!["id=token,env=TOKEN".to_string()],
            ssh: vec!["default".to_string()],
            bases: vec![],
            platform_context_keys: vec![],
            cache_from: vec!["type=registry,ref=reg/app:cache".to_string()],
            cache_to: vec!["type=inline".to_string()],
        };
        let argv = build_argv(
            "docker",
            &def,
            Path::new("/sbx/ws"),
            Path::new("/sbx/ws/app/Dockerfile"),
            Path::new("/sbx/ws/app/img.tar"),
            Path::new("/sbx/meta.json"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        );

        assert_eq!(argv[0..3], ["docker", "buildx", "build"]);
        let joined = argv.join(" ");
        assert!(joined.contains("--output type=oci,dest=/sbx/ws/app/img.tar"));
        assert!(joined.contains("--file /sbx/ws/app/Dockerfile"));
        assert!(joined.contains("--metadata-file /sbx/meta.json"));
        assert!(joined.contains("--target runtime"));
        assert!(joined.contains("--platform linux/amd64,linux/arm64"));
        let a = argv.iter().position(|x| x == "A=1").expect("A");
        let b = argv.iter().position(|x| x == "B=2").expect("B");
        assert!(a < b, "build args must be sorted: {argv:?}");
        assert!(joined.contains("--secret id=token,env=TOKEN"), "{joined}");
        assert!(joined.contains("--ssh default"), "{joined}");
        assert!(joined.contains("--cache-from type=registry,ref=reg/app:cache"));
        assert!(joined.contains("--cache-to type=inline"));
        // The context dir is the last arg, and it is the workspace root.
        assert_eq!(argv.last().expect("last"), "/sbx/ws");
    }

    /// With no explicit `platforms`, the argv states the platform the hash
    /// promised rather than letting the builder pick again at run time.
    #[test]
    fn build_argv_passes_the_resolved_builder_platform() {
        let def = OciImageDef {
            dockerfile: DockerfileSource::Path("Dockerfile".to_string()),
            out: "img.tar".to_string(),
            digest_out: "img.digest".to_string(),
            format: ImageFormat::Oci,
            build_args: BTreeMap::new(),
            stage: None,
            platforms: vec![],
            builder: None,
            secrets: vec![],
            ssh: vec![],
            bases: vec![],
            platform_context_keys: vec![],
            cache_from: vec![],
            cache_to: vec![],
        };
        let argv = build_argv(
            "docker",
            &def,
            Path::new("/c"),
            Path::new("/c/Dockerfile"),
            Path::new("/c/img.tar"),
            Path::new("/m.json"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some("linux/arm64"),
        );
        assert!(argv.join(" ").contains("--platform linux/arm64"));
    }

    #[test]
    fn build_argv_wires_named_contexts_and_src_args() {
        let def = OciImageDef {
            dockerfile: DockerfileSource::Path("Dockerfile".to_string()),
            out: "img.tar".to_string(),
            digest_out: "img.digest".to_string(),
            format: ImageFormat::Oci,
            build_args: BTreeMap::new(),
            stage: None,
            platforms: vec!["linux/amd64".to_string()],
            builder: None,
            secrets: vec![],
            ssh: vec![],
            bases: vec!["base".to_string()],
            platform_context_keys: vec![],
            cache_from: vec![],
            cache_to: vec![],
        };
        let contexts = BTreeMap::from([(
            "base".to_string(),
            "oci-layout:///sbx/ws/base/a.oci".to_string(),
        )]);
        let src = BTreeMap::from([("SRC_BIN".to_string(), "cmd/server/bin".to_string())]);
        let argv = build_argv(
            "docker",
            &def,
            Path::new("/sbx/ws"),
            Path::new("/sbx/ws/Dockerfile"),
            Path::new("/sbx/ws/img.tar"),
            Path::new("/m.json"),
            &contexts,
            &src,
            None,
        );
        let joined = argv.join(" ");
        assert!(
            joined.contains("--build-context base=oci-layout:///sbx/ws/base/a.oci"),
            "{joined}"
        );
        assert!(
            joined.contains("--build-arg SRC_BIN=cmd/server/bin"),
            "{joined}"
        );
    }

    #[test]
    fn docker_format_selects_docker_output_type() {
        let def = OciImageDef {
            dockerfile: DockerfileSource::Path("Dockerfile".to_string()),
            out: "img.tar".to_string(),
            digest_out: "img.digest".to_string(),
            format: ImageFormat::Docker,
            build_args: BTreeMap::new(),
            stage: None,
            platforms: vec![],
            builder: None,
            secrets: vec![],
            ssh: vec![],
            bases: vec![],
            platform_context_keys: vec![],
            cache_from: vec![],
            cache_to: vec![],
        };
        let argv = build_argv(
            "docker",
            &def,
            Path::new("/c"),
            Path::new("/c/Dockerfile"),
            Path::new("/c/img.tar"),
            Path::new("/m.json"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        );
        assert!(argv.join(" ").contains("type=docker,dest=/c/img.tar"));
        // `format` selects the exporter and nothing else: the build context is
        // the workspace root for a docker-format image exactly as for an OCI
        // one, so `COPY` paths mean the same thing under both.
        assert_eq!(argv.last().expect("last"), "/c");
    }

    #[test]
    fn parse_metadata_digest_extracts_containerimage_digest() {
        let meta = r#"{"containerimage.digest":"sha256:abc","image.name":"x"}"#;
        assert_eq!(parse_metadata_digest(meta).expect("digest"), "sha256:abc");
        let err = parse_metadata_digest(r#"{"image.name":"x"}"#).expect_err("missing digest");
        assert!(
            format!("{err:#}").contains("containerimage.digest"),
            "got: {err:#}"
        );
    }

    #[test]
    fn docker_load_ref_prefers_the_last_loaded_line() {
        assert_eq!(
            parse_docker_load_ref("Loaded image: alpine:latest\n").expect("ref"),
            "alpine:latest"
        );
        assert_eq!(
            parse_docker_load_ref("Loaded image ID: sha256:abc123\n").expect("id"),
            "sha256:abc123"
        );
        assert_eq!(
            parse_docker_load_ref("Loaded image: a:1\nLoaded image: b:2\n").expect("last"),
            "b:2"
        );
        assert!(parse_docker_load_ref("nothing here").is_err());
    }

    #[tokio::test]
    async fn parse_declares_context_inputs_and_two_outputs() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let ctx_inputs: Vec<&Input> = resp
            .target_def
            .inputs
            .iter()
            .filter(|i| i.origin_id.starts_with("context|"))
            .collect();
        assert_eq!(ctx_inputs.len(), 1);
        assert_eq!(ctx_inputs[0].r#ref.r#ref.format(), "//app:srcs");
        assert!(ctx_inputs[0].hashed);

        let groups: Vec<&str> = resp
            .target_def
            .outputs
            .iter()
            .map(|o| o.group.as_str())
            .collect();
        assert_eq!(groups, ["", "digest"]);
        // Named for the target: two image targets in one package must not
        // declare the same output path.
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::FilePath(p) if p == "app/img.tar"
        ));
        assert!(matches!(
            &resp.target_def.outputs[1].paths[0].content,
            Content::FilePath(p) if p == "app/img.digest"
        ));
        let def = resp.target_def.def::<OciImageDef>();
        assert_eq!(
            def.dockerfile,
            DockerfileSource::Path("app/Dockerfile".to_string())
        );
        assert_eq!(def.format, ImageFormat::Oci);
    }

    /// Two image targets in one package get distinct output paths, so a consumer
    /// of both does not hit an output collision at sandbox materialization.
    #[tokio::test]
    async fn two_image_targets_in_a_package_do_not_collide() {
        let sbx = Sandbox::new("app");
        let a = parse_in(&sbx, "//app:app", cfg(&[ctx()])).await;
        let b = parse_in(&sbx, "//app:app-debug", cfg(&[ctx()])).await;
        let path = |r: &ParseResponse| match &r.target_def.outputs[0].paths[0].content {
            Content::FilePath(p) => p.clone(),
            other => panic!("expected a file output, got {other}"),
        };
        assert_ne!(path(&a), path(&b));
    }

    /// With no explicit `platforms`, the image depends on the probe target, and
    /// the platform reaches the key as that dep's content hash. Without the dep
    /// an arm64 host and an amd64 host compute the same key for different image
    /// bytes — the remote key carries no arch segment.
    #[tokio::test]
    async fn empty_platforms_depends_on_the_probe() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;
        let probe = resp
            .target_def
            .inputs
            .iter()
            .find(|i| i.origin_id == PLATFORM_ORIGIN)
            .expect("a builder-platform probe input");
        assert_eq!(probe.r#ref.r#ref.format(), "//@heph/oci:platform");
        assert!(probe.hashed, "the probe must feed the key");
        assert!(probe.runtime, "run() reads the platform out of it");
    }

    /// Deps that are not `context` must land outside the build context. Without
    /// an unpack root they materialize at the sandbox ws dir, which is what
    /// buildx is handed as the context — so heph's probe file and a base layout
    /// (hundreds of MB) would both ride into the user's build.
    #[tokio::test]
    async fn non_context_deps_stay_out_of_the_build_context() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "bases",
                    Value::Map(HashMap::from([(
                        "base".to_string(),
                        Value::String(":alpine".to_string()),
                    )])),
                ),
            ]),
        )
        .await;

        for input in &resp.target_def.inputs {
            let in_context = input.origin_id.starts_with("context|");
            let annotated = input.annotations.contains_key("unpack_root");
            assert_eq!(
                annotated, !in_context,
                "{}: a context dep belongs in the context and nothing else does",
                input.origin_id
            );
        }
    }

    /// A named `builder` gets its own probe: two builders answer with two
    /// different default platforms, so they must not share one answer.
    #[tokio::test]
    async fn a_named_builder_probes_its_own_platform() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(
            &sbx,
            "//app:img",
            cfg(&[ctx(), ("builder", Value::String("multi".to_string()))]),
        )
        .await;
        let probe = resp
            .target_def
            .inputs
            .iter()
            .find(|i| i.origin_id == PLATFORM_ORIGIN)
            .expect("a builder-platform probe input");
        assert_eq!(
            probe.r#ref.r#ref.format(),
            "//@heph/oci:platform@builder=multi"
        );
    }

    /// An explicit `platforms` needs no probe: no dep, and nothing to run.
    #[tokio::test]
    async fn explicit_platforms_skips_the_probe() {
        let sbx = Sandbox::new("app");
        // No fake installed under this name: were `parse` still shelling out,
        // this would fail to spawn rather than pass.
        let d = Driver::with_binary(sbx.dir.path().join("absent-docker").to_string_lossy());
        let resp = d
            .parse(
                parse_req(
                    "//app:img",
                    cfg(&[
                        ctx(),
                        (
                            "platforms",
                            Value::List(vec![Value::String("linux/amd64".to_string())]),
                        ),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .expect("explicit platforms must not need the builder");
        assert!(
            !resp
                .target_def
                .inputs
                .iter()
                .any(|i| i.origin_id == PLATFORM_ORIGIN)
        );
    }

    /// `builder` reaches buildx as `--builder`. Without it a multi-platform
    /// build is stuck with whatever builder the shell happens to have selected,
    /// which is exactly what the cleared environment forbids.
    #[test]
    fn build_argv_selects_the_named_builder() {
        let def = OciImageDef {
            dockerfile: DockerfileSource::Path("Dockerfile".to_string()),
            out: "img.tar".to_string(),
            digest_out: "img.digest".to_string(),
            format: ImageFormat::Oci,
            build_args: BTreeMap::new(),
            stage: None,
            platforms: vec!["linux/amd64".to_string(), "linux/arm64".to_string()],
            builder: Some("multi".to_string()),
            secrets: vec![],
            ssh: vec![],
            bases: vec![],
            platform_context_keys: vec![],
            cache_from: vec![],
            cache_to: vec![],
        };
        let argv = build_argv(
            "docker",
            &def,
            Path::new("/c"),
            Path::new("/c/Dockerfile"),
            Path::new("/c/img.tar"),
            Path::new("/m.json"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        );
        assert!(argv.join(" ").contains("--builder multi"), "{argv:?}");
    }

    /// Two builders can produce different images from the same context — a
    /// different BuildKit, a different default platform, a different layer
    /// cache — so they must not share a cache entry.
    #[tokio::test]
    async fn hash_differs_per_builder() {
        let sbx = Sandbox::new("app");
        let plat = || {
            (
                "platforms",
                Value::List(vec![Value::String("linux/amd64".to_string())]),
            )
        };
        let a = parse_in(&sbx, "//app:img", cfg(&[ctx(), plat()])).await;
        let b = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                plat(),
                ("builder", Value::String("multi".to_string())),
            ]),
        )
        .await;
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    fn pbp(entries: &[(&str, &[(&str, &str)])]) -> (&'static str, Value) {
        let map: HashMap<String, Value> = entries
            .iter()
            .map(|(platform, groups)| {
                let inner: HashMap<String, Value> = groups
                    .iter()
                    .map(|(g, addr)| {
                        (
                            (*g).to_string(),
                            Value::List(vec![Value::String((*addr).to_string())]),
                        )
                    })
                    .collect();
                ((*platform).to_string(), Value::Map(inner))
            })
            .collect();
        ("context_by_platform", Value::Map(map))
    }

    fn platforms_of(list: &[&str]) -> (&'static str, Value) {
        (
            "platforms",
            Value::List(
                list.iter()
                    .map(|p| Value::String((*p).to_string()))
                    .collect(),
            ),
        )
    }

    /// Two variants of one target produce the same workspace-relative path, so
    /// they cannot both sit in `context`. Per-platform deps stage under their
    /// own prefix, and the platform is in the origin id so a hash trace names
    /// *the arm64 binary* rather than "a context dep".
    #[tokio::test]
    async fn per_platform_deps_stage_under_their_platform() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                platforms_of(&["linux/amd64", "linux/arm64"]),
                pbp(&[
                    ("linux/amd64", &[("bin", "//cmd/server:bin@v=amd")]),
                    ("linux/arm64", &[("bin", "//cmd/server:bin@v=arm")]),
                ]),
            ]),
        )
        .await;

        for (platform, want_addr) in [
            ("linux/amd64", "//cmd/server:bin@v=amd"),
            ("linux/arm64", "//cmd/server:bin@v=arm"),
        ] {
            let origin = format!("context_by_platform|{}|bin|0", platform_segment(platform));
            let input = resp
                .target_def
                .inputs
                .iter()
                .find(|i| i.origin_id == origin)
                .unwrap_or_else(|| panic!("no input for {platform}"));
            assert_eq!(input.r#ref.r#ref.format(), want_addr);
            assert!(input.hashed, "each platform's dep must feed the key");
            assert_eq!(
                input.annotations.get("unpack_root").map(String::as_str),
                Some(format!("{}_bin", platform_segment(platform)).as_str()),
                "each platform unpacks under its own root, or the two collide"
            );
        }
    }

    /// Adding or removing a platform changes the staged layout buildx sees, so
    /// it must bust the key even when every dep is otherwise identical.
    #[tokio::test]
    async fn the_platform_key_set_is_hashed() {
        let sbx = Sandbox::new("app");
        let one = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                platforms_of(&["linux/amd64"]),
                pbp(&[("linux/amd64", &[("bin", ":b")])]),
            ]),
        )
        .await;
        let two = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                platforms_of(&["linux/amd64", "linux/arm64"]),
                pbp(&[
                    ("linux/amd64", &[("bin", ":b")]),
                    ("linux/arm64", &[("bin", ":b")]),
                ]),
            ]),
        )
        .await;
        assert_ne!(one.target_def.hash, two.target_def.hash);
    }

    /// Silently dropping a platform's deps would remove a binary from an image
    /// with no signal, so every mismatch is a hard parse error naming the fix.
    #[tokio::test]
    async fn per_platform_mismatches_are_parse_errors() {
        let sbx = Sandbox::new("app");

        // A platform in the map that is not being built.
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                platforms_of(&["linux/amd64"]),
                pbp(&[("linux/arm64", &[("bin", ":b")])]),
            ]),
        )
        .await;
        assert!(err.contains("not in `platforms`"), "got: {err}");

        // A platform being built with no deps declared: without this the build
        // fails on one leg of the fan-out, deep in a BuildKit log.
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                platforms_of(&["linux/amd64", "linux/arm64"]),
                pbp(&[("linux/amd64", &[("bin", ":b")])]),
            ]),
        )
        .await;
        assert!(err.contains("no entry for it"), "got: {err}");

        // No explicit platforms: the probe answers at run time, so heph cannot
        // say which key to write.
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[ctx(), pbp(&[("linux/amd64", &[("bin", ":b")])])]),
        )
        .await;
        assert!(
            err.contains("requires an explicit `platforms"),
            "got: {err}"
        );

        // A group name in both maps would export one SRC_ arg twice.
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[
                (
                    "context",
                    Value::Map(HashMap::from([(
                        "bin".to_string(),
                        Value::List(vec![Value::String(":shared".to_string())]),
                    )])),
                ),
                platforms_of(&["linux/amd64"]),
                pbp(&[("linux/amd64", &[("bin", ":b")])]),
            ]),
        )
        .await;
        assert!(
            err.contains("both `context` and `context_by_platform`"),
            "got: {err}"
        );
    }

    /// Layer-cache refs are build optimizations: changing them must NOT change
    /// the input hash (an unchanged context stays a cache hit).
    #[tokio::test]
    async fn cache_refs_do_not_affect_hash() {
        let sbx = Sandbox::new("app");
        let a = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;
        let b = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "cache_to",
                    Value::List(vec![Value::String("type=inline".to_string())]),
                ),
            ]),
        )
        .await;
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "cache_to must not affect the input hash"
        );
    }

    /// Build args, in contrast, DO change the image → different hash.
    #[tokio::test]
    async fn build_args_affect_hash() {
        let sbx = Sandbox::new("app");
        let a = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;
        let b = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "build_args",
                    Value::Map(HashMap::from([(
                        "VERSION".to_string(),
                        Value::String("1.2".to_string()),
                    )])),
                ),
            ]),
        )
        .await;
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Reordering `secrets` produces the same image, so it must produce the same
    /// key — otherwise an edit that changes nothing forces a full rebuild.
    #[tokio::test]
    async fn secret_order_does_not_affect_hash() {
        let sbx = Sandbox::new("app");
        let list = |a: &str, b: &str| {
            (
                "secrets",
                Value::List(vec![
                    Value::String(a.to_string()),
                    Value::String(b.to_string()),
                ]),
            )
        };
        let a = parse_in(
            &sbx,
            "//app:img",
            cfg(&[ctx(), list("id=a,env=A", "id=b,env=B")]),
        )
        .await;
        let b = parse_in(
            &sbx,
            "//app:img",
            cfg(&[ctx(), list("id=b,env=B", "id=a,env=A")]),
        )
        .await;
        assert_eq!(a.target_def.hash, b.target_def.hash);
    }

    #[tokio::test]
    async fn parse_defaults_to_local_and_remote_cache() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;
        assert!(resp.target_def.cache.enabled);
        assert!(resp.target_def.cache.remote_enabled);
    }

    #[tokio::test]
    async fn parse_rejects_unknown_format() {
        let sbx = Sandbox::new("app");
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[ctx(), ("format", Value::String("tarball".to_string()))]),
        )
        .await;
        assert!(err.contains("oci"), "got: {err}");
    }

    /// An absolute Dockerfile is read from outside the sandbox and is not a
    /// declared input, so edits to it could never invalidate the cache.
    #[tokio::test]
    async fn parse_rejects_an_absolute_dockerfile() {
        let sbx = Sandbox::new("app");
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                ("dockerfile", Value::String("/etc/Dockerfile".to_string())),
            ]),
        )
        .await;
        assert!(err.contains("absolute"), "got: {err}");
    }

    /// `src=` reads a host file the docker CLI resolves against its own cwd —
    /// outside the sandbox and outside the hash.
    #[tokio::test]
    async fn parse_rejects_a_secret_with_a_src_source() {
        let sbx = Sandbox::new("app");
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "secrets",
                    Value::List(vec![Value::String("id=npmrc,src=.npmrc".to_string())]),
                ),
            ]),
        )
        .await;
        assert!(err.contains("src="), "got: {err}");
    }

    /// docker would re-split `A=B=C` and take a different pair than written.
    #[tokio::test]
    async fn parse_rejects_a_build_arg_key_containing_eq() {
        let sbx = Sandbox::new("app");
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "build_args",
                    Value::Map(HashMap::from([(
                        "A=B".to_string(),
                        Value::String("C".to_string()),
                    )])),
                ),
            ]),
        )
        .await;
        assert!(err.contains('='), "got: {err}");
    }

    /// A docker-format archive holds one image, never a manifest list.
    #[tokio::test]
    async fn parse_rejects_multi_platform_with_docker_format() {
        let sbx = Sandbox::new("app");
        let err = parse_err(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                ("format", Value::String("docker".to_string())),
                (
                    "platforms",
                    Value::List(vec![
                        Value::String("linux/amd64".to_string()),
                        Value::String("linux/arm64".to_string()),
                    ]),
                ),
            ]),
        )
        .await;
        assert!(err.contains("manifest list"), "got: {err}");
    }

    #[tokio::test]
    async fn parse_bases_declare_one_input_each() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "bases",
                    Value::Map(HashMap::from([(
                        "base".to_string(),
                        Value::String(":alpine".to_string()),
                    )])),
                ),
            ]),
        )
        .await;
        let base = resp
            .target_def
            .inputs
            .iter()
            .find(|i| i.origin_id == "base|base")
            .expect("a base input");
        assert_eq!(base.r#ref.r#ref.format(), "//app:alpine");
        assert!(base.hashed);
    }

    // ---------------------------------------------------------------
    // run(): what the driver actually does with the command it built.
    // ---------------------------------------------------------------

    /// A `bases` dep is staged as the *files* of an OCI layout, never as a
    /// directory entry, so the build context has to be recovered from the
    /// layout's `oci-layout` marker. Resolving it to the first staged path
    /// instead hands buildx a file, and every `FROM <base>` build fails.
    #[tokio::test]
    async fn run_points_a_base_context_at_the_layout_directory() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM base\n").expect("dockerfile");
        let resp = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "bases",
                    Value::Map(HashMap::from([(
                        "base".to_string(),
                        Value::String(":alpine".to_string()),
                    )])),
                ),
            ]),
        )
        .await;

        // What `oci_pull(layout = True)` leaves in the sandbox: a directory of
        // files, listed one per line, with no entry for the directory itself.
        let layout = sbx.pkg.join("alpine.oci");
        std::fs::create_dir_all(layout.join("blobs/sha256")).expect("mkdir layout");
        for f in ["oci-layout", "index.json"] {
            std::fs::write(layout.join(f), "{}").expect("layout file");
        }
        let staged = vec![
            layout.join("blobs/sha256/abc"),
            layout.join("index.json"),
            layout.join("oci-layout"),
        ];
        std::fs::write(&staged[0], "blob").expect("blob");

        let bin = sbx.fake("docker", FAKE_DOCKER_OK);
        let rid = "req".to_string();
        let req = run_request(
            &rid,
            "hashin",
            &resp.target_def,
            &sbx,
            &[("base|base", staged)],
        );
        Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .expect("run");

        let build = sbx
            .calls()
            .into_iter()
            .find(|c| c.contains("buildx build"))
            .expect("a buildx build call");
        assert!(
            build.contains(&format!(
                "--build-context base=oci-layout://{}",
                layout.to_string_lossy()
            )),
            "the base must point at the layout directory, got: {build}"
        );
    }

    /// A base that is not a layout at all (an `oci_image` tar, say) must say so
    /// rather than handing buildx a path it will fail on deep inside.
    #[tokio::test]
    async fn run_rejects_a_base_that_is_not_a_layout() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM base\n").expect("dockerfile");
        let resp = parse_in(
            &sbx,
            "//app:img",
            cfg(&[
                ctx(),
                (
                    "bases",
                    Value::Map(HashMap::from([(
                        "base".to_string(),
                        Value::String(":alpine".to_string()),
                    )])),
                ),
            ]),
        )
        .await;

        let tar = sbx.pkg.join("alpine.tar");
        std::fs::write(&tar, "not a layout").expect("tar");
        let bin = sbx.fake("docker", FAKE_DOCKER_OK);
        let rid = "req".to_string();
        let req = run_request(
            &rid,
            "hashin",
            &resp.target_def,
            &sbx,
            &[("base|base", vec![tar])],
        );
        let err = Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .err()
            .expect("a non-layout base must fail");
        assert!(
            format!("{err:#}").contains("layout = True"),
            "the error must name the fix, got: {err:#}"
        );
    }

    /// Drive `run()` end to end against a fake docker: the build is invoked, the
    /// digest is read back out of the metadata file, and it lands in the digest
    /// output.
    #[tokio::test]
    async fn run_builds_and_writes_the_digest_output() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let bin = sbx.fake("docker", FAKE_DOCKER_OK);
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .expect("run");

        let digest = std::fs::read_to_string(sbx.pkg.join("img.digest")).expect("digest file");
        assert_eq!(digest, "sha256:deadbeef");
        assert!(sbx.pkg.join("img.tar").exists(), "archive must be written");

        let build = sbx
            .calls()
            .into_iter()
            .find(|c| c.contains("buildx build"))
            .expect("a buildx build call");
        assert!(build.contains("--metadata-file"), "{build}");
        // The context arg is the workspace root, not the package dir.
        assert!(
            build.ends_with(&sbx.ws.to_string_lossy().into_owned()),
            "{build}"
        );
    }

    /// A non-zero exit surfaces the tool's own stderr — the user needs the
    /// BuildKit message, not just "exit status 1".
    /// The builder's progress has to land in the target's `log.txt`: that file is
    /// what the engine collects, what the failure box renders a tail of, and what
    /// `heph log` serves. The live `stdout`/`stderr` sinks are `None` outside an
    /// interactive run, so without this the whole build log goes nowhere.
    #[tokio::test]
    async fn run_writes_the_tool_output_to_the_target_log() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let bin = sbx.fake(
            "docker",
            "case \"$2\" in\n  inspect) echo \"Platforms: linux/amd64\"; exit 0 ;;\nesac\n\
             echo 'to-stdout'\necho '#1 [internal] load build definition' >&2\n\
             meta=\"\"; dest=\"\"\nwhile [ $# -gt 0 ]; do case \"$1\" in --metadata-file) \
             meta=\"$2\";; --output) dest=\"${2#*dest=}\";; esac; shift; done\n\
             printf '{\"containerimage.digest\":\"sha256:deadbeef\"}' > \"$meta\"\n\
             printf 'tar' > \"$dest\"\nexit 0",
        );
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let sandbox_dir = req.sandbox_dir.clone();
        Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .expect("run");

        let log = std::fs::read_to_string(sandbox_dir.join("log.txt")).expect("log.txt");
        assert!(
            log.contains("#1 [internal] load build definition"),
            "the builder's progress (stderr) must reach the log, got: {log:?}"
        );
        assert!(
            log.contains("to-stdout"),
            "the builder's stdout must reach the log too, got: {log:?}"
        );
    }

    #[tokio::test]
    async fn run_surfaces_stderr_on_failure() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let bin = sbx.fake(
            "docker",
            "case \"$2\" in inspect) echo 'Platforms: linux/arm64'; exit 0;; esac\n\
             echo 'ERROR: failed to solve: unknown instruction' >&2\nexit 1",
        );
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let err = Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .err()
            .expect("a failing build must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to solve"), "got: {msg}");
    }

    /// Metadata without `containerimage.digest` fails loudly rather than writing
    /// an empty digest output.
    /// The plain `docker` driver has no file exporters, so an `oci_image` build
    /// on it fails whatever `format` says. BuildKit names the exporter but not
    /// the remedy — the driver has to add it, or the user is left guessing.
    #[tokio::test]
    async fn run_names_the_fix_when_the_builder_has_no_file_exporter() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let bin = sbx.fake(
            "docker",
            "case \"$2\" in\n  inspect) echo \"Platforms: linux/amd64\"; exit 0 ;;\nesac\necho \
             'ERROR: failed to build: OCI exporter is not supported for the docker driver.' \
             >&2\nexit 1",
        );
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let err = Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .err()
            .expect("the build must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("docker buildx create"), "got: {msg}");
        assert!(msg.contains("builder ="), "got: {msg}");
        // BuildKit's own message stays in the chain.
        assert!(msg.contains("exporter is not supported"), "got: {msg}");
    }

    #[tokio::test]
    async fn run_fails_when_metadata_has_no_digest() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let bin = sbx.fake(
            "docker",
            r#"
case "$2" in
  inspect) echo 'Platforms: linux/arm64'; exit 0 ;;
esac
while [ $# -gt 0 ]; do
  case "$1" in
    --metadata-file) printf '{"image.name":"x"}' > "$2" ;;
    --output) printf 'tar' > "${2#*dest=}" ;;
  esac
  shift
done
exit 0
"#,
        );
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let err = Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .err()
            .expect("missing digest must fail");
        assert!(
            format!("{err:#}").contains("containerimage.digest"),
            "got: {err:#}"
        );
    }

    /// A missing binary is a routine first-run state (heph does not install
    /// docker), so it must name the tool and the remedy rather than surfacing a
    /// bare ENOENT.
    #[tokio::test]
    async fn run_names_the_missing_tool() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let absent = sbx.dir.path().join("not-installed-docker");
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let err = Driver::with_binary(absent.to_string_lossy())
            .run(req, &StdCancellationToken::new())
            .await
            .err()
            .expect("a missing binary must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("not found on PATH"), "got: {msg}");
    }

    /// A missing Dockerfile is caught before a build is attempted, and the error
    /// says how to get one into the context.
    #[tokio::test]
    async fn run_reports_a_missing_dockerfile_without_building() {
        let sbx = Sandbox::new("app");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;
        let bin = sbx.fake("docker", FAKE_DOCKER_OK);
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let err = Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await
            .err()
            .expect("no Dockerfile must fail");
        assert!(format!("{err:#}").contains("Dockerfile"), "got: {err:#}");
        assert!(
            !sbx.calls().iter().any(|c| c.contains("buildx build")),
            "no build should have been attempted"
        );
    }

    /// Cancellation kills the child and reports a cancellation, not a failure:
    /// the engine downcasts `CancelledError` and must not record an aborted
    /// target as a genuine failure.
    #[tokio::test]
    async fn run_cancellation_is_reported_as_cancelled() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        let bin = sbx.fake(
            "docker",
            "case \"$2\" in inspect) echo 'Platforms: linux/arm64'; exit 0;; esac\nsleep 30",
        );
        let ctoken = StdCancellationToken::new();
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);

        let cancel = ctoken.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            cancel.cancel();
        });

        let err = Driver::with_binary(bin)
            .run(req, &ctoken)
            .await
            .err()
            .expect("a cancelled build must error");
        assert!(
            hplugin::error::is_cancelled(&err),
            "expected CancelledError, got: {err:#}"
        );
    }

    /// The child runs with a cleared environment: a host variable that would
    /// change the build (`SOURCE_DATE_EPOCH`, `BUILDX_BUILDER`, …) must not
    /// reach it, because none of them are in the cache key.
    #[tokio::test]
    async fn run_clears_the_child_environment() {
        let sbx = Sandbox::new("app");
        std::fs::write(sbx.pkg.join("Dockerfile"), "FROM scratch\n").expect("dockerfile");
        let resp = parse_in(&sbx, "//app:img", cfg(&[ctx()])).await;

        // SAFETY: set before the child is spawned, within a single test.
        unsafe { std::env::set_var("BUILDX_BUILDER", "leaky") };
        let bin = sbx.fake(
            "docker",
            r#"
case "$2" in
  inspect) echo 'Platforms: linux/arm64'; exit 0 ;;
esac
printf 'BUILDX_BUILDER=%s\n' "${BUILDX_BUILDER-<unset>}" > "$(dirname "$0")/env.txt"
while [ $# -gt 0 ]; do
  case "$1" in
    --metadata-file) printf '{"containerimage.digest":"sha256:x"}' > "$2" ;;
    --output) printf 'tar' > "${2#*dest=}" ;;
  esac
  shift
done
exit 0
"#,
        );
        let rid = "req".to_string();
        let req = run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let out = Driver::with_binary(bin)
            .run(req, &StdCancellationToken::new())
            .await;
        unsafe { std::env::remove_var("BUILDX_BUILDER") };
        out.expect("run");

        let seen = std::fs::read_to_string(sbx.dir.path().join("env.txt")).expect("env.txt");
        assert_eq!(
            seen.trim(),
            "BUILDX_BUILDER=<unset>",
            "an unlisted host variable must not reach the build"
        );
    }
}
