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

use anyhow::Context as _;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageIndex;
use oci_client::{Client, Reference};

use super::archive::{Blob, Layout};
use super::auth::RegistryCredentials;

/// A blob as one request body, without copying a file-backed blob onto the heap.
///
/// A file is memory-mapped rather than read: a layer can be gigabytes, and a
/// mapping is backed by the page cache — pages are faulted in as the request
/// body is written and can be dropped again under pressure — so a layer's size
/// does not become the driver's resident heap.
fn blob_body(blob: &Blob) -> anyhow::Result<bytes::Bytes> {
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
    // place once complete, or an `oci-archive` input — and nothing writes to it
    // for the length of the push.
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
        // A monolithic PUT refuses an empty body (`PushNoDataError`). An empty
        // stream is the same POST and the closing PUT, with no PATCH between.
        client
            .push_blob_stream(reference, futures::stream::empty(), digest)
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
    let auth = creds.resolve(reference.resolve_registry()).await?;
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
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(&data));
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
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(b""));
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
}
