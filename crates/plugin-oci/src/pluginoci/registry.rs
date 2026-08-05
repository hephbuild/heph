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

use super::archive::{self, Layout};

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
/// Returns the digest the registry filed it under — the same value `oci_image`'s
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
            let bytes = layout.blob(&digest)?.clone();
            client
                .push_blob(&reference, bytes, &digest)
                .await
                .with_context(|| format!("push blob {digest}"))?;
        }

        // The layout's own bytes, not a re-serialization: a registry digests
        // exactly what it receives, and serde will not reproduce byte-for-byte
        // what buildx wrote (key order, spacing). Re-encoding gets
        // DIGEST_INVALID.
        let raw = layout.blob(digest)?;
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
        .map(|(platform, digest, size)| oci_client::manifest::ImageIndexEntry {
            media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
            artifact_type: None,
            digest: digest.clone(),
            size: *size,
            platform: platform.clone(),
            annotations: None,
        })
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

/// Pull `reference` into an in-memory layout.
///
/// `platform` selects one instance out of a manifest list; `None` takes the
/// whole index, which is what a base image for a multi-platform build needs.
pub(crate) async fn pull_layout(
    reference: &str,
    insecure: bool,
) -> anyhow::Result<(OciImageIndex, std::collections::HashMap<String, Vec<u8>>)> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("parse image reference {reference:?}"))?;
    let client = client(insecure);
    let auth = auth_for(&reference);

    let image = client
        .pull(
            &reference,
            &auth,
            vec![
                oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
                oci_client::manifest::IMAGE_LAYER_MEDIA_TYPE,
                oci_client::manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
            ],
        )
        .await
        .with_context(|| format!("pull {reference}"))?;

    let mut blobs = std::collections::HashMap::new();
    let config_digest = archive::sha256_digest(&image.config.data);
    blobs.insert(config_digest.clone(), image.config.data.to_vec());
    for layer in &image.layers {
        blobs.insert(archive::sha256_digest(&layer.data), layer.data.to_vec());
    }

    let manifest = image
        .manifest
        .context("registry returned no manifest for the pulled image")?;
    let manifest_bytes = serde_json::to_vec(&manifest).context("encode manifest")?;
    let manifest_digest = archive::sha256_digest(&manifest_bytes);
    blobs.insert(manifest_digest.clone(), manifest_bytes.clone());

    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        artifact_type: None,
        manifests: vec![oci_client::manifest::ImageIndexEntry {
            media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
            artifact_type: None,
            digest: manifest_digest,
            size: manifest_bytes.len() as i64,
            platform: None,
            annotations: None,
        }],
        annotations: None,
    };
    Ok((index, blobs))
}
