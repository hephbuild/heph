//! Shared fixtures for the cache-eviction tests (`gc`, `clean`).
//!
//! Both modules delete cache revisions, so both need the same three things: an
//! engine over a throwaway cache, a way to plant a revision with a *controlled*
//! `created_at` (real writes stamp wall-clock time, which recency assertions
//! cannot depend on), and a way to ask whether one is still there. Kept here so
//! the two suites cannot drift into disagreeing about what a cache revision is.

use crate::engine::local_cache::{
    MANIFEST_V1, Manifest, ManifestArtifact, ManifestArtifactContentType, ManifestArtifactEncoding,
    ManifestArtifactType,
};
use crate::engine::result_lock::ResultWriteGuard;
use crate::engine::{Config, Engine};
use hcore::hasync::StdCancellationToken;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::Arc;

/// An engine rooted in a fresh tempdir. The `TempDir` is returned, not dropped —
/// hold it for the test's lifetime or the cache disappears underneath it.
pub(crate) fn test_engine() -> (Arc<Engine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let _rt = crate::engine::test_rt_enter();
    let engine = Engine::new(Config {
        root: dir.path().to_path_buf(),
        home_dir: std::path::PathBuf::new(),
        parallelism: None,
        ..Default::default()
    })
    .expect("engine");
    (Arc::new(engine), dir)
}

/// `//pkg:<name>`.
pub(crate) fn addr(name: &str) -> Addr {
    Addr::new(PkgBuf::from("pkg"), name.to_string(), BTreeMap::new())
}

/// `//<pkg>:<name>`, for tests that need more than one package.
pub(crate) fn addr_in(pkg: &str, name: &str) -> Addr {
    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
}

/// Write a cache revision with a controlled `created_at` and artifact set, so
/// recency ordering is deterministic.
pub(crate) fn write_revision(
    engine: &Engine,
    addr: &Addr,
    hashin: &str,
    created: i64,
    artifacts: &[&str],
) {
    for name in artifacts {
        let mut w = engine
            .local_cache
            .writer(addr, hashin, name)
            .expect("writer");
        w.write_all(b"data").expect("write artifact");
        w.commit().expect("commit artifact");
    }
    let manifest = Manifest {
        version: "1.0.0".to_string(),
        target: addr.format(),
        created_at_nanos: created,
        hashin: hashin.to_string(),
        artifacts: artifacts
            .iter()
            .map(|name| ManifestArtifact {
                hashout: "ho".to_string(),
                group: String::new(),
                name: (*name).to_string(),
                size: 4,
                r#type: ManifestArtifactType::Output,
                content_type: ManifestArtifactContentType::Tar,
                encoding: ManifestArtifactEncoding::None,
            })
            .collect(),
    };
    let mut w = engine
        .local_cache
        .writer(addr, hashin, MANIFEST_V1)
        .expect("manifest writer");
    borsh::to_writer(&mut w, &manifest).expect("write manifest");
    w.commit().expect("commit manifest");
    // Barrier: ensure the write landed before callers enumerate.
    assert!(
        engine
            .local_cache
            .exists(addr, hashin, MANIFEST_V1)
            .expect("exists")
    );
}

/// Whether the revision's manifest is still in the cache.
pub(crate) fn present(engine: &Engine, addr: &Addr, hashin: &str) -> bool {
    engine
        .local_cache
        .exists(addr, hashin, MANIFEST_V1)
        .expect("exists")
}

/// Take `addr`'s write lock, blocking until it is free.
pub(crate) async fn wlock(engine: &Engine, addr: &Addr) -> ResultWriteGuard {
    engine
        .result_lock()
        .write(addr, &StdCancellationToken::new())
        .await
        .expect("write lock")
}
