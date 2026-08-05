//! Reading and writing OCI image layouts, in a tar or on disk.
//!
//! [`oci_client`] speaks the registry protocol and nothing else — it has no idea
//! what an `oci-archive` is. This module is the other half: it turns the bytes
//! `oci_image` produced into the `(manifest, config, layers)` triples a push
//! needs, and turns what a pull fetched back into a layout on disk.
//!
//! Everything here is content-addressed by the digests already in the manifest,
//! so nothing re-hashes a layer that a registry or a builder already named —
//! except where a digest is *verified*, which is called out where it happens.

use anyhow::Context as _;
use oci_client::manifest::{OciImageIndex, OciImageManifest};
use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

/// An OCI layout read into memory: the index, plus every blob by digest.
///
/// Blobs are held in memory because the layers of one image are exactly what a
/// push is about to send anyway. A layout whose blobs do not fit in memory is a
/// layout that will not fit through a registry upload either — if that stops
/// being true, this is the type that grows a streaming variant.
#[derive(Debug)]
pub(crate) struct Layout {
    pub index: OciImageIndex,
    pub blobs: HashMap<String, Vec<u8>>,
}

/// Which file inside an OCI layout is the entrypoint.
const INDEX_JSON: &str = "index.json";

impl Layout {
    /// Read an `oci-archive` (a tar of an OCI layout) or a layout directory.
    ///
    /// Both shapes are accepted because both are things this plugin produces:
    /// `oci_image` writes the tar, `oci_pull(layout = True)` writes the tree.
    pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
        if path.is_dir() {
            Self::read_dir(path)
        } else {
            Self::read_tar(path)
        }
    }

    fn read_tar(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path).with_context(|| format!("open archive {path:?}"))?;
        let mut ar = tar::Archive::new(file);
        let mut index_bytes = None;
        let mut blobs = HashMap::new();
        for entry in ar.entries().with_context(|| format!("read {path:?}"))? {
            let mut entry = entry.context("archive entry")?;
            let name = entry
                .path()
                .context("entry path")?
                .to_string_lossy()
                .into_owned();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).context("read entry")?;
            if name == INDEX_JSON {
                index_bytes = Some(buf);
            } else if let Some(digest) = blob_digest_of(&name) {
                blobs.insert(digest, buf);
            }
        }
        Self::finish(path, index_bytes, blobs)
    }

    fn read_dir(dir: &Path) -> anyhow::Result<Self> {
        let index_bytes = std::fs::read(dir.join(INDEX_JSON)).ok();
        let mut blobs = HashMap::new();
        let algos = dir.join("blobs");
        if algos.is_dir() {
            for algo in std::fs::read_dir(&algos).with_context(|| format!("read {algos:?}"))? {
                let algo = algo.context("blobs entry")?;
                let name = algo.file_name().to_string_lossy().into_owned();
                if !algo.path().is_dir() {
                    continue;
                }
                for blob in std::fs::read_dir(algo.path()).context("read blob dir")? {
                    let blob = blob.context("blob entry")?;
                    let digest = format!("{name}:{}", blob.file_name().to_string_lossy());
                    blobs.insert(digest, std::fs::read(blob.path()).context("read blob")?);
                }
            }
        }
        Self::finish(dir, index_bytes, blobs)
    }

    fn finish(
        path: &Path,
        index_bytes: Option<Vec<u8>>,
        blobs: HashMap<String, Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let index_bytes = index_bytes.with_context(|| {
            format!(
                "{path:?} has no `index.json`: it is not an OCI layout. A docker-format archive \
                 (`format = \"docker\"`) is a different shape — build with the default \
                 `format = \"oci\"` to push or pull it directly."
            )
        })?;
        let index: OciImageIndex =
            serde_json::from_slice(&index_bytes).context("parse index.json")?;
        Ok(Layout { index, blobs })
    }

    /// Every image manifest in the layout, paired with the platform the index
    /// filed it under. A single-image layout yields one entry with no platform.
    ///
    /// The platform comes back here rather than being looked up later because a
    /// buildx multi-platform image nests one index inside another and records
    /// the platform on the *inner* entry — the only place it exists.
    pub(crate) fn manifests(
        &self,
    ) -> anyhow::Result<Vec<(OciImageManifest, Option<oci_client::manifest::Platform>, String)>> {
        let mut out = Vec::new();
        for entry in &self.index.manifests {
            let raw = self.blob(&entry.digest)?;
            // An index may point at another index (a manifest list nested one
            // level, which is what buildx emits for a multi-platform build).
            if let Ok(inner) = serde_json::from_slice::<OciImageIndex>(raw)
                && !inner.manifests.is_empty()
            {
                for nested in &inner.manifests {
                    let raw = self.blob(&nested.digest)?;
                    out.push((
                        serde_json::from_slice(raw).context("parse nested manifest")?,
                        nested.platform.clone(),
                        nested.digest.clone(),
                    ));
                }
                continue;
            }
            out.push((
                serde_json::from_slice(raw).context("parse manifest")?,
                entry.platform.clone(),
                entry.digest.clone(),
            ));
        }
        Ok(out)
    }

    pub(crate) fn blob(&self, digest: &str) -> anyhow::Result<&Vec<u8>> {
        self.blobs
            .get(digest)
            .with_context(|| format!("blob {digest} is referenced but not in the layout"))
    }
}

/// `blobs/sha256/<hex>` → `sha256:<hex>`. Anything else is not a blob.
fn blob_digest_of(name: &str) -> Option<String> {
    let rest = name.strip_prefix("blobs/")?;
    let (algo, hex) = rest.split_once('/')?;
    (!hex.is_empty() && !hex.contains('/')).then(|| format!("{algo}:{hex}"))
}

/// Write a layout to `dir` in the on-disk OCI layout shape (`oci-layout`,
/// `index.json`, `blobs/<algo>/<hex>`) — what `oci_pull(layout = True)` produces
/// and what `oci_image`'s `bases` consumes.
pub(crate) fn write_layout_dir(
    dir: &Path,
    index: &OciImageIndex,
    blobs: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
    std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)
        .context("write oci-layout")?;
    std::fs::write(
        dir.join(INDEX_JSON),
        serde_json::to_vec(index).context("encode index.json")?,
    )
    .context("write index.json")?;
    for (digest, bytes) in blobs {
        let path = dir.join(blob_path(digest)?);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {parent:?}"))?;
        }
        std::fs::write(&path, bytes).with_context(|| format!("write blob {path:?}"))?;
    }
    Ok(())
}

/// Pack a layout directory into an `oci-archive` tar.
pub(crate) fn write_layout_tar(
    out: &Path,
    index: &OciImageIndex,
    blobs: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("create {out:?}"))?;
    let mut ar = tar::Builder::new(file);
    append(&mut ar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    append(
        &mut ar,
        INDEX_JSON,
        &serde_json::to_vec(index).context("encode index.json")?,
    )?;
    // Sorted: a tar's member order is part of its bytes, and the archive is a
    // cached artifact whose hash must not depend on HashMap iteration order.
    let mut digests: Vec<&String> = blobs.keys().collect();
    digests.sort();
    for digest in digests {
        let path = blob_path(digest)?;
        append(&mut ar, &path, &blobs[digest])?;
    }
    ar.finish().context("finish archive")?;
    Ok(())
}

fn append<W: std::io::Write>(
    ar: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
) -> anyhow::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    // Fixed: the archive is content-addressed by heph, and a wall-clock mtime
    // would make two identical images hash differently.
    header.set_mtime(0);
    header.set_cksum();
    ar.append_data(&mut header, name, data)
        .with_context(|| format!("append {name}"))?;
    Ok(())
}

fn blob_path(digest: &str) -> anyhow::Result<String> {
    let (algo, hex) = digest
        .split_once(':')
        .with_context(|| format!("malformed digest {digest:?}"))?;
    Ok(format!("blobs/{algo}/{hex}"))
}

/// The digest of `bytes`, in the `sha256:<hex>` form the OCI spec uses.
pub(crate) fn sha256_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_names_round_trip() {
        assert_eq!(
            blob_digest_of("blobs/sha256/abc"),
            Some("sha256:abc".into())
        );
        assert_eq!(blob_path("sha256:abc").expect("path"), "blobs/sha256/abc");
        // Not blobs.
        assert_eq!(blob_digest_of("index.json"), None);
        assert_eq!(blob_digest_of("blobs/sha256/a/b"), None);
    }

    /// A tar written twice from the same layout must be byte-identical: it is a
    /// cached artifact, so a wall-clock mtime or map iteration order leaking in
    /// would make one image hash two ways.
    #[test]
    fn archives_are_reproducible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = OciImageIndex {
            schema_version: 2,
            media_type: None,
            artifact_type: None,
            manifests: vec![],
            annotations: None,
        };
        let blobs = HashMap::from([
            ("sha256:aaa".to_string(), b"one".to_vec()),
            ("sha256:bbb".to_string(), b"two".to_vec()),
        ]);
        let a = dir.path().join("a.tar");
        let b = dir.path().join("b.tar");
        write_layout_tar(&a, &index, &blobs).expect("write a");
        write_layout_tar(&b, &index, &blobs).expect("write b");
        assert_eq!(
            std::fs::read(&a).expect("a"),
            std::fs::read(&b).expect("b"),
            "the same layout must produce the same bytes"
        );
    }

    /// What a push reads back out of what `oci_image` wrote.
    #[test]
    fn a_written_layout_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = b"{}".to_vec();
        let config_digest = sha256_digest(&config);
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": config_digest, "size": config.len()},
            "layers": []
        });
        let manifest_bytes = serde_json::to_vec(&manifest).expect("manifest");
        let manifest_digest = sha256_digest(&manifest_bytes);
        let index = OciImageIndex {
            schema_version: 2,
            media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
            artifact_type: None,
            manifests: vec![oci_client::manifest::ImageIndexEntry {
                media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
                artifact_type: None,
                digest: manifest_digest.clone(),
                size: manifest_bytes.len() as i64,
                platform: None,
                annotations: None,
            }],
            annotations: None,
        };
        let blobs = HashMap::from([(manifest_digest, manifest_bytes), (config_digest, config)]);

        let tar = dir.path().join("img.tar");
        write_layout_tar(&tar, &index, &blobs).expect("write");
        let read = Layout::read(&tar).expect("read");
        let manifests = read.manifests().expect("manifests");
        assert_eq!(manifests.len(), 1, "one image in, one image out");

        // The same layout as a directory reads identically — `bases` consumes
        // that shape and a push must accept either.
        let as_dir = dir.path().join("layout");
        write_layout_dir(&as_dir, &index, &blobs).expect("write dir");
        assert_eq!(
            Layout::read(&as_dir)
                .expect("read dir")
                .manifests()
                .expect("m")
                .len(),
            1
        );
    }

    /// A docker-format archive has no `index.json`. The error has to name the
    /// fix, because `format` is restated on the producer and the consumer and
    /// this is exactly where a mismatch surfaces.
    #[test]
    fn a_non_layout_archive_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("docker.tar");
        let file = std::fs::File::create(&path).expect("create");
        let mut ar = tar::Builder::new(file);
        append(&mut ar, "manifest.json", b"[]").expect("append");
        ar.finish().expect("finish");

        let err = format!("{:#}", Layout::read(&path).expect_err("not a layout"));
        assert!(err.contains("not an OCI layout"), "got: {err}");
        assert!(err.contains("format = \"docker\""), "got: {err}");
    }
}

/// Convert one image out of an OCI layout into a docker-format archive — the
/// shape `docker load` accepts on every daemon.
///
/// A daemon with the containerd image store reads an OCI archive directly, but
/// one without it does not, and that is still the default on plenty of
/// installs. skopeo used to hide this by converting during the copy; this is
/// that conversion, in-tree.
///
/// The substantive part is the layers. A docker archive carries them
/// *uncompressed*, while OCI layers are gzipped, so each one is inflated here —
/// the one genuinely expensive step, and the reason this is not just a manifest
/// rewrite.
pub(crate) fn write_docker_archive(
    out: &Path,
    layout: &Layout,
    manifest: &OciImageManifest,
    repo_tag: &str,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let config = layout.blob(&manifest.config.digest)?;
    let config_name = format!("{}.json", hex_of(&manifest.config.digest)?);

    let file = std::fs::File::create(out).with_context(|| format!("create {out:?}"))?;
    let mut ar = tar::Builder::new(file);
    append(&mut ar, &config_name, config)?;

    let mut layer_names = Vec::new();
    for (i, layer) in manifest.layers.iter().enumerate() {
        let raw = layout.blob(&layer.digest)?;
        // Gzip magic rather than the media type: a layer's declared type and its
        // actual encoding can disagree, and the bytes are what `docker load`
        // has to read.
        let bytes = if raw.starts_with(&[0x1f, 0x8b]) {
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(&raw[..])
                .read_to_end(&mut decoded)
                .with_context(|| format!("inflate layer {}", layer.digest))?;
            decoded
        } else {
            raw.clone()
        };
        let name = format!("{i}/layer.tar");
        append(&mut ar, &name, &bytes)?;
        layer_names.push(name);
    }

    let manifest_json = serde_json::json!([{
        "Config": config_name,
        "RepoTags": [repo_tag],
        "Layers": layer_names,
    }]);
    let manifest_bytes = serde_json::to_vec(&manifest_json).context("encode manifest.json")?;
    append(&mut ar, "manifest.json", &manifest_bytes)?;
    ar.finish().context("finish docker archive")?;
    drop(ar);
    std::io::stdout().flush().ok();
    Ok(())
}

/// The hex half of an `algo:hex` digest.
fn hex_of(digest: &str) -> anyhow::Result<&str> {
    digest
        .split_once(':')
        .map(|(_, hex)| hex)
        .with_context(|| format!("malformed digest {digest:?}"))
}
