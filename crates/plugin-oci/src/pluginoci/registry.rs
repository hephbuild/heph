//! Registry transport, in-process.
//!
//! `oci_push` and `oci_pull` used to shell out to `skopeo`. This speaks the OCI
//! distribution protocol directly ([`oci_client`], maintained by the ORAS
//! project), which removes a host binary that is standard on Linux CI and absent
//! from a stock Mac — the reason the default `format = "oci"` was awkward to
//! recommend.
//!
//! What skopeo did for free and is done here instead: resolving credentials from
//! `~/.docker/config.json` and the `docker-credential-*` helpers (see
//! [`auth_for`]), and pushing a manifest list rather than a single image when
//! the layout holds more than one platform.

use anyhow::Context as _;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageIndex;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};

use super::archive::{Blob, Layout};

/// How much of a blob is read at a time on its way to the registry. Bounded so
/// a layer's size never becomes the driver's peak memory.
const PUSH_CHUNK: usize = 512 * 1024;

/// A blob as a chunked byte stream, read lazily off disk.
///
/// `push_blob` takes the whole blob as one `Bytes`; `push_blob_stream` takes
/// this instead, which is what keeps a multi-gigabyte layer out of memory.
fn blob_stream(
    blob: &Blob,
) -> anyhow::Result<impl futures::Stream<Item = oci_client::errors::Result<bytes::Bytes>>> {
    let mut reader = blob.reader()?;
    Ok(futures::stream::poll_fn(move |_| {
        let mut buf = vec![0u8; PUSH_CHUNK];
        let mut filled = 0;
        // `read` is free to return short; keep going until the chunk is full or
        // the blob ends, so the registry sees uniform chunks.
        while let Some(rest) = buf.get_mut(filled..).filter(|r| !r.is_empty()) {
            match std::io::Read::read(&mut reader, rest) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    return std::task::Poll::Ready(Some(Err(
                        oci_client::errors::OciDistributionError::GenericError(Some(format!(
                            "read blob: {e}"
                        ))),
                    )));
                }
            }
        }
        if filled == 0 {
            return std::task::Poll::Ready(None);
        }
        buf.truncate(filled);
        std::task::Poll::Ready(Some(Ok(bytes::Bytes::from(buf))))
    }))
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
        ..Default::default()
    })
}

/// Credentials for `reference`'s registry, from the same places the docker CLI
/// looks: `$DOCKER_CONFIG`/`$HOME/.docker/config.json`, podman's `auth.json`,
/// and the `docker-credential-*` helper named by `credsStore` / `credHelpers`.
///
/// Anonymous when nothing is configured — a public pull needs no credentials,
/// and failing here would break the common case to serve the rare one.
fn auth_for(reference: &Reference) -> RegistryAuth {
    let server = reference.resolve_registry();
    match docker_credential::get_credential(server) {
        Ok(docker_credential::DockerCredential::UsernamePassword(user, pass)) => {
            RegistryAuth::Basic(user, pass)
        }
        Ok(docker_credential::DockerCredential::IdentityToken(token)) => {
            RegistryAuth::Bearer(token)
        }
        Err(e) => {
            // Not an error: an unconfigured registry is the normal case for a
            // public pull. Logged so an unexpected 401 has something to point at.
            tracing::debug!(server, error = %e, "no docker credentials; continuing anonymously");
            RegistryAuth::Anonymous
        }
    }
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
) -> anyhow::Result<String> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    let client = client(insecure);
    let auth = auth_for(&reference);
    client
        .auth(&reference, &auth, oci_client::RegistryOperation::Push)
        .await
        .with_context(|| format!("authenticate to {}", reference.resolve_registry()))?;

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
            // Streamed off disk in chunks, never read whole: a layer is the
            // largest thing this plugin touches, and `push_blob` would want it
            // as one `Bytes`.
            client
                .push_blob_stream(&reference, blob_stream(layout.blob(&digest)?)?, &digest)
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
        .iter()
        .map(
            |(platform, digest, size)| oci_client::manifest::ImageIndexEntry {
                media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
                artifact_type: None,
                digest: digest.clone(),
                size: *size,
                platform: platform.clone(),
                annotations: None,
            },
        )
        .collect();

    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        artifact_type: None,
        manifests,
        annotations: None,
    };
    client
        .push_manifest_list(&reference, &auth, index)
        .await
        .context("push manifest list")
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
) -> anyhow::Result<(OciImageIndex, super::archive::Blobs)> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    let client = client(insecure);
    let auth = auth_for(&reference);

    const ACCEPTED: &[&str] = &[
        oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE,
        oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE,
        oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
        oci_client::manifest::IMAGE_MANIFEST_MEDIA_TYPE,
    ];
    let (raw, digest) = client
        .pull_manifest_raw(&reference, &auth, ACCEPTED)
        .await
        .with_context(|| format!("pull the manifest of {reference}"))?;

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
            let index = OciImageIndex {
                schema_version: 2,
                media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
                artifact_type: None,
                manifests: vec![oci_client::manifest::ImageIndexEntry {
                    media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
                    artifact_type: None,
                    digest,
                    size: raw.len() as i64,
                    platform: None,
                    annotations: None,
                }],
                annotations: None,
            };
            return Ok((index, blobs));
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
            .pull_manifest_raw(&by_digest, &auth, ACCEPTED)
            .await
            .with_context(|| format!("pull the manifest for {}", entry.digest))?;
        let manifest: oci_client::manifest::OciImageManifest =
            serde_json::from_slice(&raw).context("parse a platform's manifest")?;
        blobs.insert(entry.digest.clone(), Blob::Bytes(raw.to_vec()));
        pull_one(&client, &reference, &manifest, blob_dir, &mut blobs).await?;
    }

    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        artifact_type: None,
        manifests: entries,
        annotations: None,
    };
    Ok((index, blobs))
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
                .is_some_and(|p| p.os.to_string() == os && p.architecture.to_string() == arch)
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    /// The upload path reads a blob in bounded chunks and reassembles to exactly
    /// the original bytes.
    ///
    /// Only the docker-gated suite drives a real push, so without this the
    /// chunking — off-by-one on the last partial chunk, a short `read` treated
    /// as EOF — would be covered by nothing that runs on every push.
    #[tokio::test]
    async fn a_blob_streams_back_in_bounded_chunks() {
        // Deliberately not a multiple of PUSH_CHUNK: the last chunk is partial,
        // which is where a length bug shows up.
        let len = PUSH_CHUNK * 2 + 7;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blob");
        std::fs::write(&path, &data).expect("write");

        for blob in [Blob::Bytes(data.clone()), Blob::File(path)] {
            let mut stream = Box::pin(blob_stream(&blob).expect("stream"));
            let mut chunks = Vec::new();
            let mut got = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.expect("chunk");
                chunks.push(chunk.len());
                got.extend_from_slice(&chunk);
            }
            assert_eq!(got, data, "the stream must reassemble to the blob");
            assert_eq!(
                chunks,
                vec![PUSH_CHUNK, PUSH_CHUNK, 7],
                "full chunks, then the remainder — never a chunk past the end"
            );
        }
    }
}
