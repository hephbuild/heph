//! Guest adapter: wrap a host [`DynExecutor`] back into an [`hplugin::provider::
//! ProviderExecutor`] so plugin code calls back exactly as in-process — the calls
//! are direct stabby vtable dispatch into the host, no serialization.

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use hcore::hartifactcontent::{Content, WalkEntry};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hplugin::eresult::{ArtifactMeta, EResult};
use hplugin::provider::ProviderExecutor;
use hplugin_stabby::abi::{
    DynArtifact, DynExecutor, DynRead, StableAddr, StableArtifactContentDyn, StableExecutorDyn,
    StableReadDyn,
};
use prost::Message;
use std::io::Read;
use std::sync::Arc;

/// Forwards `ProviderExecutor` calls to the host over the stable ABI.
pub struct GuestExecutor {
    exec: DynExecutor,
}

impl GuestExecutor {
    pub fn new(exec: DynExecutor) -> Self {
        Self { exec }
    }
}

/// Build the seam's [`StableAddr`] from an `Addr` — no `//pkg:name` format.
fn stable_addr(a: &Addr) -> StableAddr {
    StableAddr {
        package: a.package.as_str().into(),
        name: a.name.clone().into(),
        args: a
            .args
            .iter()
            .map(|(k, v)| hplugin_stabby::abi::StableArg {
                key: k.clone().into(),
                val: v.clone().into(),
            })
            .collect(),
    }
}

impl ProviderExecutor for GuestExecutor {
    fn note_dep<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, Result<()>> {
        // Synchronous stable call — no boxed future across the seam.
        let r = self.exec.note_dep(stable_addr(addr));
        Box::pin(async move {
            if r.ok {
                Ok(())
            } else if r.cycle {
                Err(anyhow::Error::new(hplugin::error::CycleError {
                    from: addr.clone(),
                    to: addr.clone(),
                }))
            } else {
                anyhow::bail!("{}", r.message)
            }
        })
    }

    fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, Result<Arc<EResult>>> {
        Box::pin(async move {
            let r = self.exec.result(stable_addr(addr)).await;
            if r.ok {
                let mut artifacts: Vec<Arc<dyn Content>> = Vec::with_capacity(r.artifacts.len());
                let mut meta = Vec::with_capacity(r.artifacts.len());
                for a in r.artifacts {
                    let hashout = a.hashout().to_string();
                    artifacts.push(Arc::new(StableContent { handle: a }) as Arc<dyn Content>);
                    meta.push(ArtifactMeta { hashout });
                }
                Ok(Arc::new(EResult {
                    artifacts,
                    support_artifacts: vec![],
                    artifacts_meta: meta,
                }))
            } else if r.cycle {
                Err(anyhow::Error::new(hplugin::error::CycleError {
                    from: addr.clone(),
                    to: addr.clone(),
                }))
            } else if r.cancelled {
                Err(anyhow::Error::new(hplugin::error::CancelledError))
            } else {
                anyhow::bail!("{}", r.message)
            }
        })
    }

    fn query<'a>(
        &'a self,
        m: &'a Matcher,
        extra_skip: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Addr>>> {
        Box::pin(async move {
            let matcher_pb = plugin_abi::convert::matcher_to_pb(m).encode_to_vec();
            let skip: stabby::vec::Vec<stabby::string::String> =
                extra_skip.iter().map(|s| s.clone().into()).collect();
            let r = self
                .exec
                .query(stabby::vec::Vec::from(matcher_pb.as_slice()), skip)
                .await;
            if r.ok {
                let mut out = Vec::with_capacity(r.addrs.len());
                for a in r.addrs.iter() {
                    out.push(hmodel::htaddr::parse_addr(a)?);
                }
                Ok(out)
            } else {
                anyhow::bail!("{}", r.message)
            }
        })
    }
}

/// A host artifact, streamed guest-side over the stable ABI: `reader()` opens a
/// fresh streaming reader on the host handle (no buffering); `walk()` runs the tar
/// walker over that stream (artifacts are tar — the only content the cache
/// produces today). The handle owns the host `Content` for its lifetime.
struct StableContent {
    handle: DynArtifact,
}

impl Content for StableContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        // In-process: if the host artifact is a real on-disk file, open it
        // directly — no per-chunk hop across the seam. Empty path => stream over
        // the handle (synthetic / non-file content).
        let p = self.handle.path();
        if !p.is_empty() {
            return Ok(Box::new(
                std::fs::File::open(&*p).with_context(|| format!("open artifact file {p}"))?,
            ));
        }
        Ok(Box::new(GuestRead::new(self.handle.open())))
    }
    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        Ok(Box::new(hcore::hartifactcontent::tar::TarWalker::new(
            self.reader()?,
        )?))
    }
    fn seekable_reader(
        &self,
    ) -> Result<Option<Box<dyn hcore::hartifactcontent::ReadSeek + Send>>> {
        // A file-backed artifact is seekable directly; the stream handle is not.
        let p = self.handle.path();
        if p.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(
            std::fs::File::open(&*p).with_context(|| format!("open artifact file {p}"))?,
        )))
    }
    fn hashout(&self) -> Result<String> {
        Ok(self.handle.hashout().to_string())
    }
    fn byte_size(&self) -> Option<u64> {
        match self.handle.byte_size() {
            u64::MAX => None,
            n => Some(n),
        }
    }
}

/// Adapts a stable [`DynRead`] (chunk-pull) to `std::io::Read`: serves the current
/// chunk via a `Cursor`, pulling the next chunk when it drains; an empty chunk is EOF.
struct GuestRead {
    inner: DynRead,
    cur: std::io::Cursor<Vec<u8>>,
}

impl GuestRead {
    fn new(inner: DynRead) -> Self {
        Self {
            inner,
            cur: std::io::Cursor::new(Vec::new()),
        }
    }
}

impl Read for GuestRead {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.cur.read(out)?;
            if n > 0 {
                return Ok(n);
            }
            // Current chunk drained — pull the next; empty = EOF.
            let chunk = self.inner.read_chunk();
            if chunk.is_empty() {
                return Ok(0);
            }
            self.cur = std::io::Cursor::new(chunk.to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hartifactcontent::WalkEntry;
    use hmodel::htaddr::parse_addr;
    use hplugin_stabby::host::HostExecutor;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockExec {
        note_deps: AtomicU32,
    }
    impl ProviderExecutor for MockExec {
        fn note_dep<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, Result<()>> {
            self.note_deps.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
        fn result<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, Result<Arc<EResult>>> {
            Box::pin(async {
                Ok(Arc::new(EResult {
                    artifacts: vec![Arc::new(Bytes(big_payload())) as Arc<dyn Content>],
                    support_artifacts: vec![],
                    artifacts_meta: vec![ArtifactMeta {
                        hashout: "h1".into(),
                    }],
                }))
            })
        }
        fn query<'a>(
            &'a self,
            _m: &'a Matcher,
            _s: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<Addr>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct Bytes(Vec<u8>);
    impl Content for Bytes {
        fn reader(&self) -> Result<Box<dyn Read>> {
            Ok(Box::new(std::io::Cursor::new(self.0.clone())))
        }
        fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
            anyhow::bail!("no walk in test")
        }
        fn hashout(&self) -> Result<String> {
            Ok("h1".into())
        }
    }

    /// A payload spanning several 64 KiB stream chunks, to prove the streaming
    /// reader reassembles across chunk boundaries (not whole-buffered, not truncated).
    fn big_payload() -> Vec<u8> {
        (0..200_000u32).map(|i| (i % 251) as u8).collect()
    }

    // The hot path crosses the stable ABI (host adapter -> stabby dyn -> guest
    // adapter) and back, same process — proving the conversions before the cdylib.
    #[test]
    fn hot_path_roundtrip() {
        let mock = Arc::new(MockExec {
            note_deps: AtomicU32::new(0),
        });
        let dynexec = HostExecutor::wrap(mock.clone() as Arc<dyn ProviderExecutor>);
        let guest = GuestExecutor::new(dynexec);
        let addr = parse_addr("//pkg/a:b").expect("parse addr");

        futures::executor::block_on(guest.note_dep(&addr)).expect("note_dep");
        assert_eq!(mock.note_deps.load(Ordering::Relaxed), 1);

        let eres = futures::executor::block_on(guest.result(&addr)).expect("result");
        assert_eq!(eres.artifacts.len(), 1);
        assert_eq!(eres.artifacts_meta[0].hashout, "h1");
        // The artifact streams across the seam in chunks; the guest reader must
        // reproduce the full payload exactly.
        let mut buf = Vec::new();
        eres.artifacts[0]
            .reader()
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, big_payload());
        // A second reader() opens a fresh stream (reader is re-openable).
        let mut buf2 = Vec::new();
        eres.artifacts[0]
            .reader()
            .unwrap()
            .read_to_end(&mut buf2)
            .unwrap();
        assert_eq!(buf2, big_payload());
    }

    struct MockExecFile {
        path: std::path::PathBuf,
    }
    impl ProviderExecutor for MockExecFile {
        fn note_dep<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn result<'a>(&'a self, _addr: &'a Addr) -> BoxFuture<'a, Result<Arc<EResult>>> {
            let path = self.path.clone();
            Box::pin(async move {
                Ok(Arc::new(EResult {
                    artifacts: vec![Arc::new(FileBacked { path }) as Arc<dyn Content>],
                    support_artifacts: vec![],
                    artifacts_meta: vec![ArtifactMeta {
                        hashout: "h2".into(),
                    }],
                }))
            })
        }
        fn query<'a>(
            &'a self,
            _m: &'a Matcher,
            _s: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<Addr>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    /// A file-backed artifact: its byte stream deliberately errors, so reading it
    /// at all proves the consumer went through `file_path`, not `reader()`/`open()`.
    struct FileBacked {
        path: std::path::PathBuf,
    }
    impl Content for FileBacked {
        fn reader(&self) -> Result<Box<dyn Read>> {
            anyhow::bail!("file-backed artifact must be read via file_path, not the stream")
        }
        fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
            anyhow::bail!("no walk in test")
        }
        fn hashout(&self) -> Result<String> {
            Ok("h2".into())
        }
        fn file_path(&self) -> Option<std::path::PathBuf> {
            Some(self.path.clone())
        }
    }

    // A file-backed result artifact crosses the seam as a PATH: the guest opens the
    // real file directly rather than streaming chunks over the vtable. (The host
    // Content's stream bails, so any successful read proves the path was used.)
    #[test]
    fn file_backed_artifact_reads_from_disk_not_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("art.bin");
        std::fs::write(&path, big_payload()).expect("write artifact");

        let mock = Arc::new(MockExecFile { path });
        let dynexec = HostExecutor::wrap(mock as Arc<dyn ProviderExecutor>);
        let guest = GuestExecutor::new(dynexec);
        let addr = parse_addr("//pkg/a:b").expect("parse addr");

        let eres = futures::executor::block_on(guest.result(&addr)).expect("result");
        assert_eq!(eres.artifacts.len(), 1);

        let mut buf = Vec::new();
        eres.artifacts[0]
            .reader()
            .expect("reader opens the file directly")
            .read_to_end(&mut buf)
            .expect("read");
        assert_eq!(buf, big_payload(), "bytes must match the on-disk file");

        // seekable_reader() also serves the real file (enables tar-index/FUSE paths).
        let mut seek = eres.artifacts[0]
            .seekable_reader()
            .expect("seekable_reader")
            .expect("Some for file-backed");
        let mut buf2 = Vec::new();
        seek.read_to_end(&mut buf2).expect("read seekable");
        assert_eq!(buf2, big_payload());
    }
}
