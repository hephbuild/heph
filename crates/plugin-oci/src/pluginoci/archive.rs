//! Reading and writing OCI image layouts, in a tar or on disk.
//!
//! [`oci_client`] speaks the registry protocol and nothing else — it has no idea
//! what an `oci-archive` is. This module is the other half: it turns the bytes
//! `docker_build` produced into the `(manifest, config, layers)` triples a push
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
    /// `docker_build` writes the tar, `oci_pull(layout = True)` writes the tree.
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
    ) -> anyhow::Result<
        Vec<(
            OciImageManifest,
            Option<oci_client::manifest::Platform>,
            String,
        )>,
    > {
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

/// A blob on its way *into* a layout: either bytes already in hand, or a file to
/// be copied without ever holding it.
///
/// Manifests, indexes and configs are kilobytes and stay [`Blob::Bytes`]. A
/// layer is not: `oci_image` builds one per `oci_layer` dep and a single layer
/// can be most of an image, so it travels as [`Blob::File`] and is streamed at
/// write time. Reading one into a `Vec<u8>` would put the whole image in memory
/// once per concurrent image target.
#[derive(Debug, Clone)]
pub(crate) enum Blob {
    Bytes(Vec<u8>),
    File(std::path::PathBuf),
}

impl Blob {
    fn len(&self) -> anyhow::Result<u64> {
        match self {
            Blob::Bytes(b) => Ok(b.len() as u64),
            Blob::File(p) => Ok(std::fs::metadata(p)
                .with_context(|| format!("stat blob file {p:?}"))?
                .len()),
        }
    }

    fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
        match self {
            Blob::Bytes(b) => Ok(Box::new(std::io::Cursor::new(b.clone()))),
            Blob::File(p) => Ok(Box::new(
                std::fs::File::open(p).with_context(|| format!("open blob file {p:?}"))?,
            )),
        }
    }
}

/// The blobs of a layout, keyed by digest. `BTreeMap` rather than `HashMap`
/// because the write order is the archive's bytes.
pub(crate) type Blobs = std::collections::BTreeMap<String, Blob>;

/// `blobs/sha256/<hex>` → `sha256:<hex>`. Anything else is not a blob.
fn blob_digest_of(name: &str) -> Option<String> {
    let rest = name.strip_prefix("blobs/")?;
    let (algo, hex) = rest.split_once('/')?;
    (!hex.is_empty() && !hex.contains('/')).then(|| format!("{algo}:{hex}"))
}

/// Give the layout's entry point a `ref.name` annotation, wrapping a
/// multi-platform set in a nested index first.
///
/// buildx resolves `--build-context name=oci-layout://<dir>` by looking for a
/// tag: without `org.opencontainers.image.ref.name` it reports "could not be
/// resolved: failed to resolve digest" and the `FROM name` fails. skopeo used to
/// supply this implicitly through `oci:<dir>:latest`.
///
/// A single image is annotated in place. Several are wrapped in one index —
/// buildx's own shape for a multi-platform image — so the tag names the *set*
/// and the per-platform entries keep their platforms.
fn tagged_index(index: &OciImageIndex, blobs: &mut Blobs) -> anyhow::Result<OciImageIndex> {
    const REF_NAME: &str = "org.opencontainers.image.ref.name";
    let mut annotations = std::collections::BTreeMap::new();
    annotations.insert(REF_NAME.to_string(), "latest".to_string());

    if let [only] = index.manifests.as_slice() {
        let mut entry = only.clone();
        let mut ann = entry.annotations.unwrap_or_default();
        ann.entry(REF_NAME.to_string())
            .or_insert_with(|| "latest".to_string());
        entry.annotations = Some(ann);
        return Ok(OciImageIndex {
            manifests: vec![entry],
            ..index.clone()
        });
    }

    let inner = OciImageIndex {
        schema_version: 2,
        media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        artifact_type: None,
        manifests: index.manifests.clone(),
        annotations: None,
    };
    let raw = serde_json::to_vec(&inner).context("encode the nested index")?;
    let digest = sha256_digest(&raw);
    let size = raw.len() as i64;
    blobs.insert(digest.clone(), Blob::Bytes(raw));

    Ok(OciImageIndex {
        schema_version: 2,
        media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        artifact_type: None,
        manifests: vec![oci_client::manifest::ImageIndexEntry {
            media_type: oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
            artifact_type: None,
            digest,
            size,
            platform: None,
            annotations: Some(annotations),
        }],
        annotations: None,
    })
}

/// Write a layout to `dir` in the on-disk OCI layout shape (`oci-layout`,
/// `index.json`, `blobs/<algo>/<hex>`) — what `oci_pull(layout = True)` produces
/// and what `docker_build`'s `bases` consumes.
pub(crate) fn write_layout_dir(
    dir: &Path,
    index: &OciImageIndex,
    blobs: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    write_layout_dir_blobs(dir, index, &owned(blobs))
}

/// [`write_layout_dir`], for blobs that may still be on disk.
pub(crate) fn write_layout_dir_blobs(
    dir: &Path,
    index: &OciImageIndex,
    blobs: &Blobs,
) -> anyhow::Result<()> {
    let mut blobs = blobs.clone();
    let index = &tagged_index(index, &mut blobs)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
    std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)
        .context("write oci-layout")?;
    std::fs::write(
        dir.join(INDEX_JSON),
        serde_json::to_vec(index).context("encode index.json")?,
    )
    .context("write index.json")?;
    for (digest, blob) in &blobs {
        let path = dir.join(blob_path(digest)?);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {parent:?}"))?;
        }
        // Written beside the final name and renamed, never in place: a blob's
        // *name* asserts its digest, and nothing here or in `Layout::read` ever
        // re-hashes one. A write interrupted by Ctrl-C or a full disk would
        // otherwise leave a truncated file whose name claims a digest it does
        // not have — served from the cache forever, and rejected only at the far
        // end of a registry push.
        let tmp = path.with_extension("partial");
        let mut src = blob.reader()?;
        let mut dst =
            std::fs::File::create(&tmp).with_context(|| format!("create blob {tmp:?}"))?;
        let n = std::io::copy(&mut src, &mut dst)
            .with_context(|| format!("write blob {digest} to {tmp:?}"))?;
        let want = blob.len()?;
        anyhow::ensure!(
            n == want,
            "blob {digest} changed size while being written ({n} bytes, expected {want}); \
             the layout would be corrupt"
        );
        drop(dst);
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("move blob {tmp:?} into place at {path:?}"))?;
    }
    Ok(())
}

/// Pack a layout directory into an `oci-archive` tar.
pub(crate) fn write_layout_tar(
    out: &Path,
    index: &OciImageIndex,
    blobs: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    write_layout_tar_blobs(out, index, &owned(blobs))
}

/// [`write_layout_tar`], for blobs that may still be on disk.
pub(crate) fn write_layout_tar_blobs(
    out: &Path,
    index: &OciImageIndex,
    blobs: &Blobs,
) -> anyhow::Result<()> {
    let mut blobs = blobs.clone();
    let index = &tagged_index(index, &mut blobs)?;
    let file = std::fs::File::create(out).with_context(|| format!("create {out:?}"))?;
    let mut ar = tar::Builder::new(file);
    append(&mut ar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    append(
        &mut ar,
        INDEX_JSON,
        &serde_json::to_vec(index).context("encode index.json")?,
    )?;
    // `Blobs` is a BTreeMap: a tar's member order is part of its bytes, and the
    // archive is a cached artifact whose hash must not depend on map iteration
    // order.
    for (digest, blob) in &blobs {
        append_blob(&mut ar, &blob_path(digest)?, blob)?;
    }
    ar.finish().context("finish archive")?;
    Ok(())
}

/// Wrap already-in-memory blobs. The callers that have them (`oci_pull`,
/// `oci_push`) hold whole images anyway — the registry upload they feed is the
/// same bytes — so nothing is lost by keeping their shape.
fn owned(blobs: &HashMap<String, Vec<u8>>) -> Blobs {
    blobs
        .iter()
        .map(|(d, b)| (d.clone(), Blob::Bytes(b.clone())))
        .collect()
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

/// [`append`] for a blob that may still be on disk — streamed, never buffered.
fn append_blob<W: std::io::Write>(
    ar: &mut tar::Builder<W>,
    name: &str,
    blob: &Blob,
) -> anyhow::Result<()> {
    if let Blob::Bytes(bytes) = blob {
        return append(ar, name, bytes);
    }
    let mut header = tar::Header::new_gnu();
    header.set_size(blob.len()?);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    ar.append_data(&mut header, name, blob.reader()?)
        .with_context(|| format!("append {name}"))?;
    Ok(())
}

/// The sha256 of a file, without reading it into memory.
pub(crate) fn sha256_file(path: &Path) -> anyhow::Result<(String, u64)> {
    use sha2::{Digest as _, Sha256};
    let mut f = std::fs::File::open(path).with_context(|| format!("open {path:?}"))?;
    let mut h = Sha256::new();
    let n = std::io::copy(&mut f, &mut h).with_context(|| format!("hash {path:?}"))?;
    Ok((format!("sha256:{:x}", h.finalize()), n))
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

/// Write one image out of an OCI layout as a docker-format archive — the shape
/// `docker load` accepts on every daemon, including those without the
/// containerd image store.
///
/// This is what `buildx --output type=docker` itself emits, and copying that
/// exactly is the point: a *hybrid*. The OCI layout is written unchanged, and a
/// `manifest.json` is added beside it naming the config and layers by their
/// `blobs/sha256/<hex>` paths. Layers stay gzipped — the daemon reads them as
/// they are, so there is nothing to inflate and no digest to recompute.
///
/// A single instance, not the whole index: a daemon tag holds one image.
pub(crate) fn write_docker_archive(
    out: &Path,
    layout: &Layout,
    manifest: &OciImageManifest,
    manifest_digest: &str,
    repo_tag: &str,
) -> anyhow::Result<()> {
    // Only the blobs this instance needs — a multi-arch layout would otherwise
    // carry every architecture's layers into a tag that holds one image.
    let mut blobs = HashMap::new();
    let manifest_bytes = layout.blob(manifest_digest)?.clone();
    blobs.insert(manifest_digest.to_string(), manifest_bytes.clone());
    blobs.insert(
        manifest.config.digest.clone(),
        layout.blob(&manifest.config.digest)?.clone(),
    );
    for layer in &manifest.layers {
        blobs.insert(layer.digest.clone(), layout.blob(&layer.digest)?.clone());
    }

    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        artifact_type: None,
        manifests: vec![oci_client::manifest::ImageIndexEntry {
            media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
            artifact_type: None,
            digest: manifest_digest.to_string(),
            size: manifest_bytes.len() as i64,
            platform: None,
            annotations: None,
        }],
        annotations: None,
    };

    let docker_manifest = serde_json::json!([{
        "Config": blob_path(&manifest.config.digest)?,
        "RepoTags": [repo_tag],
        "Layers": manifest.layers.iter()
            .map(|l| blob_path(&l.digest))
            .collect::<anyhow::Result<Vec<_>>>()?,
    }]);

    let file = std::fs::File::create(out).with_context(|| format!("create {out:?}"))?;
    let mut ar = tar::Builder::new(file);
    append(&mut ar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    append(
        &mut ar,
        INDEX_JSON,
        &serde_json::to_vec(&index).context("encode index.json")?,
    )?;
    let mut digests: Vec<&String> = blobs.keys().collect();
    digests.sort();
    for digest in digests {
        let bytes = blobs
            .get(digest)
            .with_context(|| format!("blob {digest} vanished between listing and writing"))?;
        append(&mut ar, &blob_path(digest)?, bytes)?;
    }
    append(
        &mut ar,
        "manifest.json",
        &serde_json::to_vec(&docker_manifest).context("encode manifest.json")?,
    )?;
    ar.finish().context("finish docker archive")?;
    Ok(())
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

    /// What a push reads back out of what `docker_build` wrote.
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
