//! Registry transport, in-process.
//!
//! `oci_push` and `oci_pull` used to shell out to `skopeo`. This speaks the OCI
//! distribution protocol directly ([`oci_client`], maintained by the ORAS
//! project), which removes a host binary that is standard on Linux CI and absent
//! from a stock Mac — the reason the default `format = "oci"` was awkward to
//! recommend.
//!
//! What skopeo did for free and is done here instead: resolving credentials —
//! from a target's declared `credentials`, or from `~/.docker/config.json` and
//! the `docker-credential-*` helpers when it declares none (see [`super::auth`])
//! — and pushing a manifest list rather than a single image when the layout
//! holds more than one platform.
//!
//! Also the manifest-only operations on an image already in a registry: a
//! lookup ([`manifest_ref`], for `oci_exists`) and a retag ([`tag`], for
//! `oci_tag`).

use anyhow::Context as _;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageIndex;
use oci_client::{Client, Reference};

use super::archive::{self, Blob, Layout};
use super::auth::RegistryCredentials;

/// A blob as one request body, without copying a file-backed blob onto the heap.
///
/// A file is memory-mapped rather than read: a layer can be gigabytes, and a
/// mapping is backed by the page cache — pages are faulted in as the request
/// body is written and can be dropped again under pressure — so a layer's size
/// does not become the driver's resident heap.
pub(super) fn blob_body(blob: &Blob) -> anyhow::Result<bytes::Bytes> {
    let (path, offset, len) = match blob {
        // Manifests and configs: kilobytes, already in memory.
        Blob::Bytes(b) => return Ok(bytes::Bytes::from(b.clone())),
        Blob::File(path) => (path, 0, blob.len()?),
        Blob::FileRange { path, offset, len } => (path, *offset, *len),
    };
    if len == 0 {
        return Ok(bytes::Bytes::new());
    }
    let file = std::fs::File::open(path).with_context(|| format!("open blob file {path:?}"))?;
    let len = usize::try_from(len).with_context(|| format!("blob {path:?} is {len} bytes"))?;
    // SAFETY: a mapping is only sound while nothing truncates or rewrites the
    // file under it. This file is a content-addressed blob in the sandbox this
    // driver owns — a layer staged by `oci_image`, a pulled blob renamed into
    // place once complete, an `oci-archive` input, or the docker-format archive
    // `oci_load` wrote and closed before handing it to the daemon — and nothing
    // writes to it for the length of the push or the load.
    let map = unsafe {
        memmap2::MmapOptions::new()
            .offset(offset)
            .len(len)
            .map(&file)
    }
    .with_context(|| format!("map blob file {path:?} ({len} bytes at {offset})"))?;
    Ok(bytes::Bytes::from_owner(map))
}

/// Upload one blob in a single request: POST to open the upload, then one PUT
/// carrying the whole body.
///
/// Not the chunked upload (a PATCH per chunk, then PUT): Google Artifact
/// Registry answers a PATCH with a `Location` that refuses the next PATCH with
/// 405, so every blob over one chunk failed to push. A monolithic upload never
/// sends a second PATCH, and every registry must accept it.
async fn push_blob(
    client: &Client,
    reference: &Reference,
    blob: &Blob,
    digest: &str,
) -> anyhow::Result<()> {
    let body = blob_body(blob)?;
    if body.is_empty() {
        // `push_blob` refuses an empty body (`PushNoDataError`). An empty stream
        // with a declared size of 0 is the same POST and the closing PUT, with
        // `Content-Length: 0` and nothing in between. The size is mandatory
        // since oci-client 0.18: with `use_monolithic_push` it is the header.
        client
            .push_blob_stream(reference, futures::stream::empty(), digest, Some(0))
            .await?;
    } else {
        client.push_blob(reference, body, digest).await?;
    }
    Ok(())
}

/// Build a client for `insecure` (plain HTTP / self-signed) or the default TLS.
fn client(insecure: bool) -> Client {
    Client::new(ClientConfig {
        protocol: if insecure {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        accept_invalid_certificates: insecure,
        // See `push_blob`: `push_blob` with this set is POST + one PUT.
        use_monolithic_push: true,
        ..Default::default()
    })
}

/// Push every image in `layout` to `reference`, and a manifest list when there
/// is more than one.
///
/// Returns the digest the registry filed it under — the same value `docker_build`'s
/// `digest` output group carries, so a caller can compare them.
pub(crate) async fn push_layout(
    layout: &Layout,
    reference: &str,
    insecure: bool,
    creds: &RegistryCredentials<'_>,
) -> anyhow::Result<String> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    let client = client(insecure);
    let auth = authenticate(
        &client,
        &reference,
        creds,
        &[oci_client::RegistryOperation::Push],
    )
    .await?;

    let manifests = layout.manifests()?;
    anyhow::ensure!(
        !manifests.is_empty(),
        "the image layout holds no manifests; there is nothing to push"
    );

    let mut entries = Vec::new();
    for (manifest, platform, digest) in &manifests {
        // Blobs first: a manifest naming a blob the registry does not have is
        // rejected. `blob_exists` is what makes a re-push of an unchanged image
        // cheap — the registry already has every layer.
        let mut blobs = vec![manifest.config.digest.clone()];
        blobs.extend(manifest.layers.iter().map(|l| l.digest.clone()));
        for digest in blobs {
            if client
                .blob_exists(&reference, &digest)
                .await
                .unwrap_or(false)
            {
                continue;
            }
            push_blob(&client, &reference, layout.blob(&digest)?, &digest)
                .await
                .with_context(|| format!("push blob {digest}"))?;
        }

        // The layout's own bytes, not a re-serialization: a registry digests
        // exactly what it receives, and serde will not reproduce byte-for-byte
        // what buildx wrote (key order, spacing). Re-encoding gets
        // DIGEST_INVALID.
        let raw = layout.blob_bytes(digest)?;
        client
            .push_manifest_raw(
                &reference,
                raw.clone(),
                manifest
                    .media_type
                    .clone()
                    .unwrap_or_else(|| oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string())
                    .parse()
                    .context("manifest media type")?,
            )
            .await
            .context("push image manifest")?;
        entries.push((platform.clone(), digest.clone(), raw.len() as i64));
    }

    // One image: its manifest is what the tag points at. More than one: the tag
    // has to point at a list, or a puller on the other architecture finds
    // nothing.
    if entries.len() == 1 {
        return Ok(entries.remove(0).1);
    }

    // Built from what was actually pushed, not from the layout's own top-level
    // entries: for a buildx multi-platform image those point at a *nested*
    // index, and a list naming a digest the registry never received is rejected
    // with MANIFEST_BLOB_UNKNOWN.
    let manifests = entries
        .into_iter()
        .map(|(platform, digest, size)| archive::image_manifest_entry(digest, size, platform))
        .collect();

    let index = archive::oci_index(manifests);
    client
        .push_manifest_list(&reference, &auth, index)
        .await
        .context("push manifest list")
}

/// `<registry>/<repository>@<digest>` — the ref with its tag replaced by what
/// the registry actually served.
///
/// Built from the *resolved* registry and repository rather than by editing the
/// user's string: `alpine:3.20` and `docker.io/library/alpine:3.20` name one
/// image, and only the expanded form is unambiguous to paste back into `src`.
/// `reference` in full, the way the push addressed it: registry, repository and
/// tag, with the tag the registry assumed (`latest`) written out when none was
/// given, Docker Hub shorthand expanded, and a digest kept when there was one.
pub(crate) fn full_ref(reference: &str) -> anyhow::Result<String> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    Ok(reference.whole())
}

fn pinned_ref(reference: &Reference, digest: &str) -> String {
    // `registry()`, not `resolve_registry()`: the former keeps the spelling the
    // BUILD file used and already normalizes Docker Hub shorthand to
    // `docker.io`, while the latter reports the *API endpoint*
    // (`index.docker.io`) — correct to connect to, needlessly surprising to
    // paste back.
    format!(
        "{}/{}@{}",
        reference.registry().trim_end_matches('/'),
        reference.repository(),
        digest
    )
}

/// The manifest types every manifest request accepts: an image or a list, OCI or
/// Docker.
///
/// All four, always. A registry answers with the type it stored when the client
/// accepts it, and converts otherwise: Docker Hub serves a manifest *list* as the
/// one instance it picks when the list type is missing from `Accept`. A tag copy
/// that took that instance would retag one platform of a multi-platform image.
const MANIFEST_TYPES: &[&str] = &[
    oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE,
    oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE,
    oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
    oci_client::manifest::IMAGE_MANIFEST_MEDIA_TYPE,
];

/// Whether `e` is the registry saying the manifest or the repository is not
/// there, as opposed to any other failure.
///
/// Narrow on purpose. A 401 or 403 is not "missing": many registries answer an
/// identity that cannot read a repository that way whether it exists or not, and
/// reading that as absence would let an expired credential report a published
/// image as missing.
///
/// Two shapes count. A bare 404 (`ServerError`, a body that is no error
/// envelope). And an error envelope whose every code says "unknown": the client
/// turns any failed response with an envelope into `RegistryError`, which does
/// not carry the HTTP status, so an envelope is classified by its codes alone.
fn is_not_found(e: &oci_client::errors::OciDistributionError) -> bool {
    use oci_client::errors::{OciDistributionError as E, OciErrorCode as C};
    match e {
        E::ServerError { code: 404, .. } => true,
        E::RegistryError { envelope, .. } => {
            !envelope.errors.is_empty()
                && envelope.errors.iter().all(|err| {
                    matches!(err.code, C::ManifestUnknown | C::NameUnknown | C::NotFound)
                })
        }
        _ => false,
    }
}

/// Authenticate `client` for each of `ops` on `reference`'s repository, up
/// front, and return the credential resolved for its registry.
///
/// The manifest calls would authenticate on their own, but they swallow a
/// failed token exchange and go on anonymously, so a rejected credential comes
/// back as a 401 — or, from a registry that hides private repositories, as a
/// 404 that [`manifest_ref`] would report as a missing image. Exchanging first
/// makes a token request the registry refuses an error that says so. A
/// registry that challenges with Basic has no exchange to make: it checks the
/// credential on the request itself, which then fails with a 401.
///
/// Every op a caller makes a request for is named: the client caches a token
/// per operation, and one it was not handed here is fetched lazily, with the
/// failure swallowed.
async fn authenticate(
    client: &Client,
    reference: &Reference,
    creds: &RegistryCredentials<'_>,
    ops: &[oci_client::RegistryOperation],
) -> anyhow::Result<oci_client::secrets::RegistryAuth> {
    let auth = creds.resolve(reference.resolve_registry()).await?;
    for &op in ops {
        client
            .auth(reference, &auth, op)
            .await
            .with_context(|| format!("authenticate to {}", reference.resolve_registry()))?;
    }
    Ok(auth)
}

/// What a lookup found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// The pinned reference (`registry/repository@digest`, see [`pinned_ref`])
    /// of the manifest.
    Found(String),
    /// The registry says there is no such manifest or repository; holds what
    /// it answered, for the error the caller writes.
    Missing(String),
}

/// Look up the manifest `reference` names.
///
/// A `HEAD` on the manifest: nothing is downloaded when the registry sends the
/// digest header, which every mainstream one does. Any failure other than "not
/// found" (see [`is_not_found`]) is an error, never [`Lookup::Missing`].
pub(crate) async fn manifest_ref(
    reference: &str,
    insecure: bool,
    creds: &RegistryCredentials<'_>,
) -> anyhow::Result<Lookup> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    let client = client(insecure);
    let auth = authenticate(
        &client,
        &reference,
        creds,
        &[oci_client::RegistryOperation::Pull],
    )
    .await?;
    match client.fetch_manifest_digest(&reference, &auth).await {
        Ok(digest) => Ok(Lookup::Found(pinned_ref(&reference, &digest))),
        Err(e) if is_not_found(&e) => {
            tracing::debug!(%reference, error = %e, "the registry has no such manifest");
            Ok(Lookup::Missing(e.to_string()))
        }
        Err(e) => Err(e).with_context(|| format!("look up the manifest of {reference}")),
    }
}

/// The media type to push a manifest's bytes back as.
///
/// Its own `mediaType` when it has one, which Docker manifests always do and
/// OCI ones should. Without it the OCI image spec tells the two shapes apart
/// by `manifests`, which only an index has.
fn manifest_media_type(raw: &[u8]) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct Shape {
        #[serde(rename = "mediaType")]
        media_type: Option<String>,
        manifests: Option<serde::de::IgnoredAny>,
    }
    let shape: Shape = serde_json::from_slice(raw).context("parse the manifest")?;
    Ok(match (shape.media_type, shape.manifests) {
        (Some(t), _) => t,
        (None, Some(_)) => oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
        (None, None) => oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
    })
}

/// Point each of `tags` in `reference`'s repository at the manifest `reference`
/// names, and return its pinned reference (see [`pinned_ref`]).
///
/// The manifest's own bytes are pushed back under each tag, unchanged, so every
/// tag resolves to the same digest — a re-serialized manifest would get a new
/// one. Nothing else moves: the blobs and, for a list, the per-platform
/// manifests are already in the repository, which is why a tag can only be
/// added in the repository the image is in.
///
/// A tag that already exists is moved, silently. The tags move one `PUT` at a
/// time, so a failure partway leaves the earlier ones moved: the error names
/// them, and a re-run moves the rest (pointing a tag where it already points
/// is a no-op).
pub(crate) async fn tag(
    reference: &str,
    tags: &[String],
    insecure: bool,
    creds: &RegistryCredentials<'_>,
) -> anyhow::Result<String> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    let client = client(insecure);
    // Pull for the manifest fetch, push for the tags: both up front, or the
    // fetch's token is exchanged lazily and a refusal is swallowed.
    let auth = authenticate(
        &client,
        &reference,
        creds,
        &[
            oci_client::RegistryOperation::Pull,
            oci_client::RegistryOperation::Push,
        ],
    )
    .await?;

    let (raw, digest) = match client
        .pull_manifest_raw(&reference, &auth, MANIFEST_TYPES)
        .await
    {
        Ok(found) => found,
        Err(e) if is_not_found(&e) => anyhow::bail!(
            "{reference} is not in the registry, so there is nothing to tag: push it first ({e})"
        ),
        Err(e) => return Err(e).with_context(|| format!("pull the manifest of {reference}")),
    };
    let media_type =
        manifest_media_type(&raw).with_context(|| format!("read the manifest of {reference}"))?;
    let content_type = media_type
        .parse()
        .with_context(|| format!("manifest media type {media_type:?}"))?;

    let mut moved: Vec<&str> = Vec::with_capacity(tags.len());
    for tag in tags {
        let dest: Reference = format!("{}/{}:{tag}", reference.registry(), reference.repository())
            .parse()
            .with_context(|| format!("build the reference for tag {tag:?}"))?;
        client
            .push_manifest_raw(&dest, raw.clone(), Clone::clone(&content_type))
            .await
            .with_context(|| {
                let moved = if moved.is_empty() {
                    "none".to_string()
                } else {
                    moved.join(", ")
                };
                format!("point {dest} at {digest}; already moved: {moved} (a re-run is idempotent)")
            })?;
        moved.push(tag);
    }
    Ok(pinned_ref(&reference, &digest))
}

/// What a pull resolved to, beyond the bytes.
pub(crate) struct Pulled {
    pub index: OciImageIndex,
    pub blobs: super::archive::Blobs,
    /// See [`pinned_ref`] — e.g. `cgr.dev/chainguard/static@sha256:…`.
    ///
    /// A tag is a pointer, and heph keys a pull on the *string* — so a caller
    /// who wants the pull to stay reproducible needs the thing the pointer
    /// pointed at, at the moment it was followed. This is that, spelled so it
    /// can be pasted straight back into `src`.
    pub pinned_ref: String,
}

/// Pull the selected platforms of `reference` into an in-memory layout.
///
/// Goes through the raw manifest rather than `Client::pull`, which resolves a
/// multi-platform index against the *client's own* default platform — on an
/// arm64 mac that matches nothing in a `linux/*` index, and it gives the caller
/// no way to ask for a platform, let alone several.
pub(crate) async fn pull_layout(
    reference: &str,
    platforms: &super::pull::PlatformSelect,
    insecure: bool,
    blob_dir: &std::path::Path,
    creds: &RegistryCredentials<'_>,
) -> anyhow::Result<Pulled> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    let client = client(insecure);
    let auth = creds.resolve(reference.resolve_registry()).await?;

    let (raw, digest) = client
        .pull_manifest_raw(&reference, &auth, MANIFEST_TYPES)
        .await
        .with_context(|| format!("pull the manifest of {reference}"))?;

    let pinned_ref = pinned_ref(&reference, &digest);

    std::fs::create_dir_all(blob_dir)
        .with_context(|| format!("create the blob staging dir {blob_dir:?}"))?;
    let mut blobs = super::archive::Blobs::new();

    // An index: choose among its instances. A bare manifest: there is nothing to
    // choose, and asking for a platform it does not advertise would be pedantry.
    let entries = match serde_json::from_slice::<OciImageIndex>(&raw) {
        Ok(index) if !index.manifests.is_empty() => {
            blobs.insert(digest.clone(), Blob::Bytes(raw.to_vec()));
            select_entries(&index, platforms)?
        }
        _ => {
            let manifest: oci_client::manifest::OciImageManifest =
                serde_json::from_slice(&raw).context("parse image manifest")?;
            blobs.insert(digest.clone(), Blob::Bytes(raw.to_vec()));
            pull_one(&client, &reference, &manifest, blob_dir, &mut blobs).await?;
            let index = archive::oci_index(vec![archive::image_manifest_entry(
                digest,
                raw.len() as i64,
                None,
            )]);
            return Ok(Pulled {
                index,
                blobs,
                pinned_ref,
            });
        }
    };

    for entry in &entries {
        let by_digest: Reference = format!(
            "{}/{}@{}",
            reference.resolve_registry(),
            reference.repository(),
            entry.digest
        )
        .parse()
        .context("build a digest reference")?;
        let (raw, _) = client
            .pull_manifest_raw(&by_digest, &auth, MANIFEST_TYPES)
            .await
            .with_context(|| format!("pull the manifest for {}", entry.digest))?;
        let manifest: oci_client::manifest::OciImageManifest =
            serde_json::from_slice(&raw).context("parse a platform's manifest")?;
        blobs.insert(entry.digest.clone(), Blob::Bytes(raw.to_vec()));
        pull_one(&client, &reference, &manifest, blob_dir, &mut blobs).await?;
    }

    let index = archive::oci_index(entries);
    Ok(Pulled {
        index,
        blobs,
        pinned_ref,
    })
}

/// The index entries the selection asks for, or an error naming what is on offer.
fn select_entries(
    index: &OciImageIndex,
    platforms: &super::pull::PlatformSelect,
) -> anyhow::Result<Vec<oci_client::manifest::ImageIndexEntry>> {
    let wanted = match platforms {
        super::pull::PlatformSelect::All => return Ok(index.manifests.clone()),
        super::pull::PlatformSelect::Only(wanted) => wanted,
    };

    let available: Vec<String> = index
        .manifests
        .iter()
        .filter_map(|e| e.platform.as_ref())
        .map(|p| format!("{}/{}", p.os, p.architecture))
        .collect();

    let mut out = Vec::new();
    for want in wanted {
        let (os, arch) = super::split_platform(want)?;
        let hit = index.manifests.iter().find(|e| {
            e.platform
                .as_ref()
                .is_some_and(|p| super::is_os_arch(p, os, arch))
        });
        match hit {
            Some(entry) => out.push(entry.clone()),
            // Loud: a silently-missing platform produces a layout that fails
            // much later, inside someone else's build.
            None => anyhow::bail!(
                "{want} is not published for this image (it has: {}). Pick one of those, or set \
                 `all_platforms = True` to take whatever the registry has.",
                available.join(", ")
            ),
        }
    }
    Ok(out)
}

/// Fetch one manifest's config and layers, straight to disk.
///
/// Streamed rather than pulled into a `Vec<u8>`: a pull's whole job is to
/// produce an artifact on disk, and buffering every layer first meant a
/// multi-gigabyte base image was resident in the plugin before a single byte of
/// it was written.
///
/// The chunks are written with `std::fs`, not `tokio::fs`, on purpose: a plugin
/// cdylib's tokio is a separate runtime instance polled by host workers, so
/// anything that reaches for a reactor or a blocking pool aborts across the ABI
/// seam. Reading from the network is the host's socket; writing is a plain
/// `write_all`.
async fn pull_one(
    client: &Client,
    reference: &Reference,
    manifest: &oci_client::manifest::OciImageManifest,
    blob_dir: &std::path::Path,
    blobs: &mut super::archive::Blobs,
) -> anyhow::Result<()> {
    use futures::StreamExt as _;
    use std::io::Write as _;

    let mut wanted = vec![manifest.config.digest.clone()];
    wanted.extend(manifest.layers.iter().map(|l| l.digest.clone()));
    for digest in wanted {
        if blobs.contains_key(&digest) {
            // Shared between platforms more often than not — a base layer is
            // the same blob for every architecture that inherits it.
            continue;
        }
        let path = blob_dir.join(digest.replace(':', "_"));
        // Written to a temp name and renamed: the file is content-addressed by
        // its digest and nothing re-verifies it, so an interrupted pull must not
        // leave a truncated blob behind claiming to be the whole thing.
        let tmp = path.with_extension("partial");
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create blob file {tmp:?}"))?;
        let mut stream = client
            .pull_blob_stream(reference, digest.as_str())
            .await
            .with_context(|| format!("pull blob {digest}"))?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("read blob {digest} from the registry"))?;
            file.write_all(&chunk)
                .with_context(|| format!("write blob {digest} to {tmp:?}"))?;
        }
        file.flush().with_context(|| format!("flush {tmp:?}"))?;
        drop(file);
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("move blob {tmp:?} into place at {path:?}"))?;
        blobs.insert(digest, Blob::File(path));
    }
    Ok(())
}

/// An in-memory registry speaking enough of the distribution protocol for the
/// manifest operations: `HEAD`/`GET`/`PUT` on `/v2/<repo>/manifests/<ref>`,
/// open, behind Basic auth, or behind a Bearer token exchange.
#[cfg(test)]
pub(crate) mod testreg {
    use sha2::Digest as _;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::sync::{Arc, Mutex};

    /// What the presented test credential answers, and the header it becomes.
    pub(crate) const BOT_ANSWER: &str = r#"{"Username":"bot","Secret":"s3cret-material"}"#;
    pub(crate) const BOT_BASIC: &str = "Basic Ym90OnMzY3JldC1tYXRlcmlhbA==";
    /// The same identity with the wrong secret: what an expired or rotated
    /// password looks like.
    pub(crate) const BOT_WRONG_ANSWER: &str = r#"{"Username":"bot","Secret":"stale"}"#;
    /// The token a bearer-mode registry's `/token` grants.
    pub(crate) const BEARER_TOKEN: &str = "mock-token";

    /// How the registry authenticates a request.
    #[derive(Clone, Copy)]
    enum Auth {
        Open,
        /// Every request carries this `Authorization`, or gets a 401 with a
        /// Basic challenge.
        Basic(&'static str),
        /// The token flow: `/v2/` challenges with `Bearer realm=…/token`,
        /// `/token` grants [`BEARER_TOKEN`] to a request carrying this Basic
        /// `Authorization` (and a 401 to anything else), and a manifest request
        /// needs `Bearer <token>`. A manifest read without it is a 404, as a
        /// registry that hides private repositories answers.
        Bearer(&'static str),
    }

    /// One request received.
    #[derive(Clone, Debug)]
    pub(crate) struct Request {
        /// `METHOD path`, the query included.
        pub line: String,
        pub authorization: Option<String>,
    }

    #[derive(Default)]
    struct State {
        /// `(repo, tag or digest)` → `(content type, bytes)`.
        manifests: BTreeMap<(String, String), (String, Vec<u8>)>,
        /// `(repo, tag)` a `PUT` is refused on, as an immutable tag is.
        immutable: BTreeSet<(String, String)>,
        requests: Vec<Request>,
        /// The `scope` of every `/token` request, decoded.
        scopes: Vec<String>,
    }

    pub(crate) struct MockRegistry {
        port: u16,
        state: Arc<Mutex<State>>,
    }

    /// A one-config, no-layer OCI image manifest, with its `mediaType`.
    pub(crate) fn oci_manifest() -> Vec<u8> {
        serde_json::json!({
            "schemaVersion": 2,
            "mediaType": oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                "size": 2,
            },
            "layers": [],
        })
        .to_string()
        .into_bytes()
    }

    pub(crate) fn digest_of(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
    }

    fn content_type_of(bytes: &[u8]) -> String {
        super::manifest_media_type(bytes).expect("a manifest the test wrote")
    }

    impl MockRegistry {
        /// `basic`: the `Authorization` every request must carry, or `None`
        /// for an open registry.
        pub(crate) fn start(basic: Option<&'static str>) -> Self {
            Self::start_with(basic.map_or(Auth::Open, Auth::Basic))
        }

        /// A registry behind the token flow (see [`Auth::Bearer`]): `/token`
        /// grants a token only to a request carrying `basic`.
        pub(crate) fn start_bearer(basic: &'static str) -> Self {
            Self::start_with(Auth::Bearer(basic))
        }

        fn start_with(auth: Auth) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let state = Arc::new(Mutex::new(State::default()));
            let shared = Arc::clone(&state);
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { return };
                    let state = Arc::clone(&shared);
                    std::thread::spawn(move || serve(conn, auth, port, &state));
                }
            });
            MockRegistry { port, state }
        }

        /// Refuse every `PUT` on `tag` in `repo`, as a registry refuses to move
        /// an immutable tag.
        pub(crate) fn make_immutable(&self, repo: &str, tag: &str) {
            self.state
                .lock()
                .expect("lock")
                .immutable
                .insert((repo.to_string(), tag.to_string()));
        }

        pub(crate) fn host(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        /// Store `bytes` under `tag` (and its digest) in `repo`; returns the digest.
        pub(crate) fn put(&self, repo: &str, tag: &str, bytes: &[u8]) -> String {
            let digest = digest_of(bytes);
            let entry = (content_type_of(bytes), bytes.to_vec());
            let mut s = self.state.lock().expect("lock");
            s.manifests
                .insert((repo.to_string(), tag.to_string()), entry.clone());
            s.manifests
                .insert((repo.to_string(), digest.clone()), entry);
            digest
        }

        /// The `(content type, bytes)` stored under `reference` in `repo`.
        pub(crate) fn get(&self, repo: &str, reference: &str) -> Option<(String, Vec<u8>)> {
            self.state
                .lock()
                .expect("lock")
                .manifests
                .get(&(repo.to_string(), reference.to_string()))
                .cloned()
        }

        /// Every request received, in order.
        pub(crate) fn log(&self) -> Vec<Request> {
            self.state.lock().expect("lock").requests.clone()
        }

        /// `METHOD path` of every request received, in order.
        pub(crate) fn requests(&self) -> Vec<String> {
            self.log().into_iter().map(|r| r.line).collect()
        }

        /// Every `Authorization` header received.
        pub(crate) fn authorizations(&self) -> Vec<String> {
            self.log()
                .into_iter()
                .filter_map(|r| r.authorization)
                .collect()
        }

        /// The `scope` of every `/token` request, in order.
        pub(crate) fn scopes(&self) -> Vec<String> {
            self.state.lock().expect("lock").scopes.clone()
        }
    }

    fn envelope(code: &str) -> Vec<u8> {
        serde_json::json!({"errors": [{"code": code, "message": code}]})
            .to_string()
            .into_bytes()
    }

    /// `application/x-www-form-urlencoded` decoding, enough for a scope.
    fn form_decode(s: &str) -> String {
        let mut out = Vec::with_capacity(s.len());
        let mut bytes = s.bytes();
        while let Some(b) = bytes.next() {
            match b {
                b'+' => out.push(b' '),
                b'%' => {
                    let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                    let hex = std::str::from_utf8(&hex).expect("ascii escape");
                    out.push(u8::from_str_radix(hex, 16).expect("hex escape"));
                }
                b => out.push(b),
            }
        }
        String::from_utf8(out).expect("utf-8 query")
    }

    /// The value of `key` in `query`, decoded.
    fn query_param(query: &str, key: &str) -> Option<String> {
        query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| form_decode(v))
    }

    fn serve(conn: std::net::TcpStream, auth_mode: Auth, port: u16, state: &Mutex<State>) {
        let mut out = conn.try_clone().expect("clone stream");
        let mut rd = std::io::BufReader::new(conn);
        loop {
            let mut line = String::new();
            if rd.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let (mut len, mut auth, mut ctype) = (0usize, None, String::new());
            loop {
                let mut h = String::new();
                if rd.read_line(&mut h).unwrap_or(0) == 0 {
                    return;
                }
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    let v = v.trim().to_string();
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.parse().expect("content-length");
                    } else if k.eq_ignore_ascii_case("authorization") {
                        auth = Some(v);
                    } else if k.eq_ignore_ascii_case("content-type") {
                        ctype = v;
                    }
                }
            }
            let mut body = vec![0u8; len];
            rd.read_exact(&mut body).expect("body");

            let mut s = state.lock().expect("lock");
            s.requests.push(Request {
                line: format!("{method} {path}"),
                authorization: auth.clone(),
            });
            let mut headers = Vec::new();
            let is_manifest = path.starts_with("/v2/") && path.contains("/manifests/");
            let has_token = auth
                .as_deref()
                .and_then(|a| a.strip_prefix("Bearer "))
                .is_some_and(|t| t == BEARER_TOKEN);
            let (status, resp): (&str, Vec<u8>) = if let Auth::Basic(basic) = auth_mode
                && auth.as_deref() != Some(basic)
            {
                headers.push("WWW-Authenticate: Basic realm=\"mock\"".to_string());
                ("401 Unauthorized", envelope("UNAUTHORIZED"))
            } else if let Auth::Bearer(basic) = auth_mode
                && let Some(query) = path.strip_prefix("/token?")
            {
                if let Some(scope) = query_param(query, "scope") {
                    s.scopes.push(scope);
                }
                if auth.as_deref() == Some(basic) {
                    let token = serde_json::json!({ "token": BEARER_TOKEN });
                    ("200 OK", token.to_string().into_bytes())
                } else {
                    ("401 Unauthorized", envelope("UNAUTHORIZED"))
                }
            } else if let Auth::Bearer(_) = auth_mode
                && !has_token
            {
                if is_manifest && (method == "HEAD" || method == "GET") {
                    ("404 Not Found", envelope("NAME_UNKNOWN"))
                } else {
                    headers.push(format!(
                        "WWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"mock\""
                    ));
                    ("401 Unauthorized", envelope("UNAUTHORIZED"))
                }
            } else if path == "/v2/" {
                ("200 OK", b"{}".to_vec())
            } else if let Some((repo, reference)) = path
                .strip_prefix("/v2/")
                .and_then(|p| p.split_once("/manifests/"))
            {
                let key = (repo.to_string(), reference.to_string());
                match method.as_str() {
                    "PUT" if s.immutable.contains(&key) => {
                        ("400 Bad Request", envelope("TAG_INVALID"))
                    }
                    "PUT" => {
                        let digest = digest_of(&body);
                        let entry = (ctype.clone(), body.clone());
                        s.manifests.insert(key, entry.clone());
                        s.manifests
                            .insert((repo.to_string(), digest.clone()), entry);
                        headers.push(format!("Location: /v2/{repo}/manifests/{digest}"));
                        headers.push(format!("Docker-Content-Digest: {digest}"));
                        ("201 Created", Vec::new())
                    }
                    _ => match s.manifests.get(&key) {
                        Some((ctype, bytes)) => {
                            headers.push(format!("Content-Type: {ctype}"));
                            headers.push(format!("Docker-Content-Digest: {}", digest_of(bytes)));
                            ("200 OK", bytes.clone())
                        }
                        None if s.manifests.keys().any(|(r, _)| r == repo) => {
                            ("404 Not Found", envelope("MANIFEST_UNKNOWN"))
                        }
                        None => ("404 Not Found", envelope("NAME_UNKNOWN")),
                    },
                }
            } else {
                ("404 Not Found", envelope("NOT_FOUND"))
            };
            drop(s);
            let headers: String = headers.iter().map(|h| format!("{h}\r\n")).collect();
            let head = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\n\r\n",
                resp.len()
            );
            // A HEAD answer carries the length of the body it does not send.
            let sent = out.write_all(head.as_bytes()).and_then(|()| {
                if method == "HEAD" {
                    Ok(())
                } else {
                    out.write_all(&resp)
                }
            });
            if sent.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The remedy the warning offers has to be pasteable: a ref the user can
    /// drop straight into `src`, naming the digest the tag pointed at.
    #[test]
    fn a_resolved_ref_pins_the_digest_and_keeps_the_spelling() {
        let d = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let pin = |r: &str| pinned_ref(&r.parse::<Reference>().expect("ref"), d);

        // A private registry keeps the spelling the BUILD file used.
        assert_eq!(
            pin("cgr.dev/chainguard/static:latest-glibc"),
            format!("cgr.dev/chainguard/static@{d}")
        );
        // Docker Hub shorthand expands, because `alpine` alone is not a ref any
        // other tool would resolve the same way.
        assert_eq!(
            pin("alpine:3.20"),
            format!("docker.io/library/alpine@{d}"),
            "shorthand expands: `alpine@sha256:…` is not a ref every tool agrees on"
        );
        // Already pinned: the digest is simply restated, never doubled up.
        assert_eq!(
            pin(&format!("cgr.dev/chainguard/static@{d}")),
            format!("cgr.dev/chainguard/static@{d}")
        );
        // The result is a ref again — the whole point is that it round-trips.
        assert!(
            pin("cgr.dev/chainguard/static:latest-glibc")
                .parse::<Reference>()
                .is_ok()
        );
    }

    /// What `oci_push` writes as its output: the reference it pushed to, never
    /// missing the tag the registry filed it under.
    #[test]
    fn full_ref_always_names_registry_repository_and_tag() {
        let d = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for (given, full) in [
            ("reg.io/me/app:1.2", "reg.io/me/app:1.2".to_string()),
            // No tag: the push went to `latest`, so the output says so.
            ("reg.io/me/app", "reg.io/me/app:latest".to_string()),
            // A port is not a tag.
            (
                "localhost:5000/app",
                "localhost:5000/app:latest".to_string(),
            ),
            ("localhost:5000/app:v1", "localhost:5000/app:v1".to_string()),
            // Docker Hub shorthand expands to what was actually addressed.
            ("alpine", "docker.io/library/alpine:latest".to_string()),
            ("me/app:dev", "docker.io/me/app:dev".to_string()),
            // A digest is kept, with the tag beside it when there is one.
            (
                &format!("reg.io/me/app:1.2@{d}") as &str,
                format!("reg.io/me/app:1.2@{d}"),
            ),
        ] {
            assert_eq!(full_ref(given).expect("parse"), full, "{given}");
        }
        assert!(full_ref("").is_err());
        assert!(full_ref("Reg.io/UPPER/app").is_err());
    }

    /// One request the mock registry received.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        target: String,
        body: Vec<u8>,
    }

    /// A registry that answers blob uploads the way Google Artifact Registry
    /// does: the `Location` a PATCH returns is a `/v2/…/pkg/…` URL that refuses
    /// any further PATCH with 405, though it takes the closing PUT.
    ///
    /// Plain HTTP/1.1 over a std listener, one thread per connection, keep-alive.
    /// Every request is recorded so a test can say what reached the wire.
    fn artifact_registry_mock() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        use std::io::{BufRead as _, Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut out = conn.try_clone().expect("clone stream");
                    let mut rd = std::io::BufReader::new(conn);
                    loop {
                        let mut line = String::new();
                        if rd.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut parts = line.split_whitespace();
                        let method = parts.next().unwrap_or_default().to_string();
                        let target = parts.next().unwrap_or_default().to_string();
                        let mut len = 0usize;
                        loop {
                            let mut h = String::new();
                            if rd.read_line(&mut h).unwrap_or(0) == 0 {
                                return;
                            }
                            let h = h.trim_end();
                            if h.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = h.split_once(':')
                                && k.eq_ignore_ascii_case("content-length")
                            {
                                len = v.trim().parse().expect("content-length");
                            }
                        }
                        let mut body = vec![0u8; len];
                        rd.read_exact(&mut body).expect("body");

                        let path = target.split('?').next().unwrap_or_default();
                        let (status, location) = match method.as_str() {
                            "GET" if path == "/v2/" => ("200 OK", None),
                            "POST" if path.ends_with("/blobs/uploads/") => (
                                "202 Accepted",
                                Some("/artifacts-uploads/namespaces/p/repositories/r/uploads/ID1"),
                            ),
                            "PATCH" if path.starts_with("/artifacts-uploads/") => {
                                ("202 Accepted", Some("/v2/p/r/pkg/blobs/uploads/ID2"))
                            }
                            "PATCH" => ("405 Method Not Allowed", None),
                            "PUT" if target.contains("?digest=") => {
                                ("201 Created", Some("/v2/p/r/img/blobs/sha256:stored"))
                            }
                            _ => ("404 Not Found", None),
                        };
                        log.lock().expect("log").push(Seen {
                            method,
                            target,
                            body,
                        });
                        let location = location
                            .map(|l| format!("Location: {l}\r\n"))
                            .unwrap_or_default();
                        let resp =
                            format!("HTTP/1.1 {status}\r\n{location}Content-Length: 0\r\n\r\n");
                        if out.write_all(resp.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, seen)
    }

    /// A blob larger than one upload chunk reaches a registry whose post-PATCH
    /// `Location` refuses another PATCH — Artifact Registry, observed with curl.
    ///
    /// The chunked upload (POST, a PATCH per 512 KiB, PUT) followed that
    /// `Location` into a 405 on the second PATCH, so every layer over one chunk
    /// failed to push. One request carrying the whole body never asks the
    /// registry for a second.
    #[tokio::test]
    async fn a_blob_larger_than_a_chunk_pushes_in_one_request() {
        use sha2::Digest as _;

        let (port, seen) = artifact_registry_mock();
        let client = client(true);
        let reference: Reference = format!("127.0.0.1:{port}/p/r/img:t").parse().expect("ref");

        // Past two of the old 512 KiB chunks, and not a multiple of one.
        let len: usize = 1024 * 1024 + 7;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&data)));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blob");
        std::fs::write(&path, &data).expect("write");
        // The same bytes inside a larger file, as an `oci-archive` holds them.
        let archive = dir.path().join("archive");
        let mut padded = vec![0xAAu8; 4099];
        padded.extend_from_slice(&data);
        padded.extend_from_slice(&[0xBB; 13]);
        std::fs::write(&archive, &padded).expect("write archive");

        for blob in [
            Blob::File(path),
            Blob::FileRange {
                path: archive,
                offset: 4099,
                len: len as u64,
            },
            Blob::Bytes(data.clone()),
        ] {
            seen.lock().expect("log").clear();
            push_blob(&client, &reference, &blob, &digest)
                .await
                .unwrap_or_else(|e| panic!("push {blob:?}: {e:#}"));

            let seen = seen.lock().expect("log").clone();
            let carrying: Vec<_> = seen.iter().filter(|s| !s.body.is_empty()).collect();
            assert_eq!(
                carrying.len(),
                1,
                "the whole blob goes in exactly one request, got: {:?}",
                seen.iter()
                    .map(|s| format!("{} {} ({} bytes)", s.method, s.target, s.body.len()))
                    .collect::<Vec<_>>()
            );
            let only = carrying.first().expect("one request");
            assert_eq!(only.method, "PUT", "the body rides the closing PUT");
            assert!(
                only.target
                    .contains(&format!("digest={}", digest.replace(':', "%3A"))),
                "the PUT names the digest: {}",
                only.target
            );
            assert!(only.body == data, "the registry stores exactly the blob");
        }
    }

    /// An empty blob still pushes. A monolithic upload refuses an empty body
    /// (`PushNoDataError`), so this has to take another route — POST then the
    /// closing PUT, which is what an empty upload is anyway.
    #[tokio::test]
    async fn an_empty_blob_pushes() {
        use sha2::Digest as _;

        let (port, seen) = artifact_registry_mock();
        let client = client(true);
        let reference: Reference = format!("127.0.0.1:{port}/p/r/img:t").parse().expect("ref");
        let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(b"")));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty");
        std::fs::write(&path, b"").expect("write");

        for blob in [Blob::File(path), Blob::Bytes(Vec::new())] {
            seen.lock().expect("log").clear();
            push_blob(&client, &reference, &blob, &digest)
                .await
                .unwrap_or_else(|e| panic!("push {blob:?}: {e:#}"));
            let methods: Vec<_> = seen
                .lock()
                .expect("log")
                .iter()
                .map(|s| s.method.clone())
                .collect();
            assert_eq!(methods, ["POST", "PUT"], "no PATCH for an empty blob");
        }
    }

    /// Only the registry saying "unknown" is missing. Anything an identity
    /// problem or an outage looks like is an error, so an expired credential
    /// never reports a published image as absent.
    #[test]
    fn only_an_unknown_answer_counts_as_not_found() {
        use oci_client::errors::{OciDistributionError as E, OciEnvelope, OciError, OciErrorCode};
        let env = |codes: Vec<OciErrorCode>| E::RegistryError {
            envelope: OciEnvelope {
                errors: codes
                    .into_iter()
                    .map(|code| OciError {
                        code,
                        message: String::new(),
                        detail: serde_json::Value::Null,
                    })
                    .collect(),
            },
            url: "u".into(),
        };
        let server = |code| E::ServerError {
            code,
            url: "u".into(),
            message: String::new(),
        };
        for (e, missing) in [
            (server(404), true),
            (env(vec![OciErrorCode::ManifestUnknown]), true),
            (env(vec![OciErrorCode::NameUnknown]), true),
            (env(vec![OciErrorCode::NotFound]), true),
            (server(401), false),
            (server(403), false),
            (server(500), false),
            (E::UnauthorizedError { url: "u".into() }, false),
            (E::AuthenticationFailure("no".into()), false),
            (env(vec![OciErrorCode::Denied]), false),
            (env(vec![OciErrorCode::Unauthorized]), false),
            (env(vec![OciErrorCode::Toomanyrequests]), false),
            // One unknown beside a denial is still a denial.
            (
                env(vec![OciErrorCode::ManifestUnknown, OciErrorCode::Denied]),
                false,
            ),
            (env(vec![]), false),
            (
                env(vec![OciErrorCode::Other("MANIFEST_UNKNOWN_ISH".into())]),
                false,
            ),
        ] {
            assert_eq!(is_not_found(&e), missing, "{e}");
        }
    }

    #[test]
    fn a_manifest_is_pushed_back_as_its_own_media_type() {
        use oci_client::manifest::{
            IMAGE_MANIFEST_LIST_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE,
        };
        for (raw, want) in [
            (
                format!(r#"{{"mediaType":"{IMAGE_MANIFEST_LIST_MEDIA_TYPE}","manifests":[]}}"#),
                IMAGE_MANIFEST_LIST_MEDIA_TYPE,
            ),
            // No `mediaType`: an index is told apart by its `manifests`.
            (
                r#"{"schemaVersion":2,"manifests":[]}"#.to_string(),
                OCI_IMAGE_INDEX_MEDIA_TYPE,
            ),
            (
                r#"{"schemaVersion":2,"config":{},"layers":[]}"#.to_string(),
                OCI_IMAGE_MEDIA_TYPE,
            ),
        ] {
            assert_eq!(
                manifest_media_type(raw.as_bytes()).expect(&raw),
                want,
                "{raw}"
            );
        }
        assert!(manifest_media_type(b"not json").is_err());
        assert!(manifest_media_type(br#"{"mediaType":1}"#).is_err());
    }

    /// A multi-platform list is retagged whole, as its own type, and a
    /// reference by digest works as well as one by tag.
    #[tokio::test]
    async fn a_list_is_retagged_whole_by_tag_or_digest() {
        use oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE;
        let reg = testreg::MockRegistry::start(None);
        let list = format!(
            r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST_LIST_MEDIA_TYPE}","manifests":[]}}"#
        )
        .into_bytes();
        let digest = reg.put("acme/app", "build-1", &list);
        for (source, tag) in [
            (format!("{}/acme/app:build-1", reg.host()), "by-tag"),
            (format!("{}/acme/app@{digest}", reg.host()), "by-digest"),
        ] {
            let pinned = super::tag(
                &source,
                &[tag.to_string()],
                true,
                &RegistryCredentials::None,
            )
            .await
            .unwrap_or_else(|e| panic!("{source}: {e:#}"));
            assert_eq!(pinned, format!("{}/acme/app@{digest}", reg.host()));
            let (ctype, bytes) = reg.get("acme/app", tag).expect(tag);
            assert_eq!(ctype, IMAGE_MANIFEST_LIST_MEDIA_TYPE);
            assert_eq!(bytes, list);
        }
    }

    /// A lookup is a `HEAD`: nothing is downloaded when the registry sends the
    /// digest header.
    #[tokio::test]
    async fn a_lookup_downloads_no_manifest() {
        let reg = testreg::MockRegistry::start(None);
        let digest = reg.put("acme/app", "1", &testreg::oci_manifest());
        let source = format!("{}/acme/app:1", reg.host());
        let found = manifest_ref(&source, true, &RegistryCredentials::None)
            .await
            .expect("lookup");
        assert_eq!(
            found,
            Lookup::Found(format!("{}/acme/app@{digest}", reg.host()))
        );
        assert!(
            !reg.requests().iter().any(|r| r.starts_with("GET /v2/acme")),
            "{:?}",
            reg.requests()
        );
    }
}
