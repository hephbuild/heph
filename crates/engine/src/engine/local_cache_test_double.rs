//! Shared `LocalCache` test double.
//!
//! `ForwardingCache` forwards every call to a wrapped `inner` cache, with
//! optional hooks on the handful of calls existing tests observe (`reader`,
//! `exists`, `list_target_entries`). Before this module, three call sites each
//! hand-copied the full ten-method `LocalCache` forwarding impl to instrument
//! one or two of those calls — the exact shape that let #252 happen: a method
//! added to the trait gets forwarded correctly on the copies a PR's author
//! remembers to touch, compiles green because the others still satisfy the
//! trait via whatever the last edit left them at, and only reddens whichever
//! unrelated test exercises the copy that fell behind. Consolidating the
//! forwarding into one place means there is only one copy to keep in sync.
//!
//! Add another `on_*` hook here, not another struct, if a test needs to watch
//! a different call.

use crate::engine::local_cache::{EntryWriter, Existence, LocalCache, SizedReader, TargetStream};
use hmodel::htaddr::Addr;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

type ReaderHook = Box<dyn Fn(&Addr, &str, &str) + Send + Sync>;
type ExistsHook = Box<dyn Fn(&Addr, &str, &str) + Send + Sync>;
type ListTargetEntriesHook = Box<dyn Fn(&Addr) + Send + Sync>;
type WriterHook = Arc<dyn Fn(&Addr, &str, &str) + Send + Sync>;
type ListTargetsItemHook = Arc<dyn Fn(&str) + Send + Sync>;

pub(crate) struct ForwardingCache {
    inner: Arc<dyn LocalCache>,
    on_reader: ReaderHook,
    on_exists: ExistsHook,
    on_list_target_entries: ListTargetEntriesHook,
    on_list_targets_item: Option<ListTargetsItemHook>,
    on_writer: WriterHook,
    on_writer_done: Option<WriterHook>,
}

impl ForwardingCache {
    pub(crate) fn new(inner: Arc<dyn LocalCache>) -> Self {
        Self {
            inner,
            on_reader: Box::new(|_, _, _| {}),
            on_exists: Box::new(|_, _, _| {}),
            on_list_target_entries: Box::new(|_| {}),
            on_list_targets_item: None,
            on_writer: Arc::new(|_, _, _| {}),
            on_writer_done: None,
        }
    }

    /// Run `f` before every `reader` call, then forward regardless of what it
    /// does.
    pub(crate) fn on_reader(
        mut self,
        f: impl Fn(&Addr, &str, &str) + Send + Sync + 'static,
    ) -> Self {
        self.on_reader = Box::new(f);
        self
    }

    /// Run `f` before every `exists` call, then forward regardless of what it
    /// does.
    pub(crate) fn on_exists(
        mut self,
        f: impl Fn(&Addr, &str, &str) + Send + Sync + 'static,
    ) -> Self {
        self.on_exists = Box::new(f);
        self
    }

    /// Run `f` before every `list_target_entries` call, then forward
    /// regardless of what it does.
    pub(crate) fn on_list_target_entries(
        mut self,
        f: impl Fn(&Addr) + Send + Sync + 'static,
    ) -> Self {
        self.on_list_target_entries = Box::new(f);
        self
    }

    /// Run `f` as each target key is *pulled* from the `list_targets` iterator,
    /// not when the iterator is handed out.
    ///
    /// The distinction is the point: it makes the enumeration's laziness
    /// observable, so a consumer that is supposed to interleave enumeration with
    /// work — rather than drain the whole list up front — can be held to it. A
    /// hook that fired once, at `list_targets` time, could not tell the two
    /// apart.
    pub(crate) fn on_list_targets_item(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_list_targets_item = Some(Arc::new(f));
        self
    }

    /// Run `f` when a `writer` is opened, then forward regardless of what it
    /// does.
    pub(crate) fn on_writer(
        mut self,
        f: impl Fn(&Addr, &str, &str) + Send + Sync + 'static,
    ) -> Self {
        self.on_writer = Arc::new(f);
        self
    }

    /// Run `f` when a writer handed out by `writer` is finished with — committed,
    /// or dropped uncommitted — i.e. when that write has *completed*, not merely
    /// started. Lets ordering tests assert "B opened only after A finished"
    /// rather than the weaker "after A opened". The hook also fires when a write
    /// is abandoned on an error path, after the inner writer has discarded it.
    pub(crate) fn on_writer_done(
        mut self,
        f: impl Fn(&Addr, &str, &str) + Send + Sync + 'static,
    ) -> Self {
        self.on_writer_done = Some(Arc::new(f));
        self
    }
}

/// Forwards writes; fires the `on_writer_done` hook exactly once, when the write
/// finishes — on commit, or on drop for an abandoned write. Either way the inner
/// writer is finalized (committed or discarded) *before* the hook runs, so a
/// hook that probes the cache observes the write's final state.
struct DoneWriter {
    inner: Option<Box<dyn EntryWriter>>,
    done: Option<Box<dyn FnOnce() + Send>>,
}

impl io::Write for DoneWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.as_mut() {
            Some(w) => w.write(buf),
            None => Err(io::Error::other("writer already finalized")),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self.inner.as_mut() {
            Some(w) => w.flush(),
            None => Ok(()),
        }
    }
}

impl EntryWriter for DoneWriter {
    fn commit(mut self: Box<Self>) -> anyhow::Result<()> {
        let res = match self.inner.take() {
            Some(w) => w.commit(),
            None => Ok(()),
        };
        if let Some(done) = self.done.take() {
            done();
        }
        res
    }
}

impl Drop for DoneWriter {
    fn drop(&mut self) {
        // Discard the inner writer before signaling done — see the type doc.
        drop(self.inner.take());
        if let Some(done) = self.done.take() {
            done();
        }
    }
}

impl LocalCache for ForwardingCache {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<SizedReader> {
        (self.on_reader)(addr, hashin, name);
        self.inner.reader(addr, hashin, name)
    }

    fn writer(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> anyhow::Result<Box<dyn EntryWriter>> {
        (self.on_writer)(addr, hashin, name);
        let inner = self.inner.writer(addr, hashin, name)?;
        Ok(match &self.on_writer_done {
            Some(done) => {
                let done = Arc::clone(done);
                let (addr, hashin, name) = (addr.clone(), hashin.to_string(), name.to_string());
                Box::new(DoneWriter {
                    inner: Some(inner),
                    done: Some(Box::new(move || done(&addr, &hashin, &name))),
                })
            }
            None => inner,
        })
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool> {
        (self.on_exists)(addr, hashin, name);
        self.inner.exists(addr, hashin, name)
    }

    fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Existence> {
        self.inner.existence(addr, hashin, name)
    }

    fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool> {
        self.inner.exists_committed(addr, hashin, name)
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<()> {
        self.inner.delete(addr, hashin, name)
    }

    fn list_targets(&self) -> anyhow::Result<TargetStream> {
        let inner = self.inner.list_targets()?;
        let Some(hook) = self.on_list_targets_item.as_ref().map(Arc::clone) else {
            return Ok(inner);
        };
        // Wrapped lazily so the hook fires per `next()`, preserving whatever
        // laziness the inner stream has.
        Ok(Box::new(inner.inspect(move |item| {
            if let Ok(key) = item {
                hook(key);
            }
        })))
    }

    fn list_target_entries(&self, addr: &Addr) -> anyhow::Result<Vec<String>> {
        (self.on_list_target_entries)(addr);
        self.inner.list_target_entries(addr)
    }

    fn seekable_reader(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> anyhow::Result<Option<Box<dyn hcore::hartifactcontent::ReadSeek + Send>>> {
        self.inner.seekable_reader(addr, hashin, name)
    }

    fn file_path(&self, addr: &Addr, hashin: &str, name: &str) -> Option<PathBuf> {
        self.inner.file_path(addr, hashin, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Config, Engine};
    use hmodel::htpkg::PkgBuf;
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn addr() -> Addr {
        Addr::new(PkgBuf::from("pkg"), "t".to_string(), BTreeMap::new())
    }

    fn real_cache(dir: &std::path::Path) -> Arc<dyn LocalCache> {
        let _rt = crate::engine::test_rt_enter();
        let engine = Engine::new(Config {
            root: dir.to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        engine.local_cache.clone()
    }

    // A hook that is never installed must not fire — otherwise `on_exists`
    // firing on a `reader` call (or vice versa) would silently miscount every
    // caller that only wires up one hook, exactly the kind of cross-wiring
    // bug this double exists to keep out of every call site that reaches for
    // it.
    #[test]
    fn unset_hooks_are_silent_and_set_hooks_fire_only_their_own_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = addr();

        let reader_calls = Arc::new(AtomicUsize::new(0));
        let exists_calls = Arc::new(AtomicUsize::new(0));

        let cache = ForwardingCache::new(real_cache(dir.path()))
            .on_reader({
                let c = Arc::clone(&reader_calls);
                move |_, _, _| {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            })
            .on_exists({
                let c = Arc::clone(&exists_calls);
                move |_, _, _| {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            });
        // `on_list_target_entries` deliberately left unset.

        let mut w = cache.writer(&a, "h", "out").expect("writer");
        w.write_all(b"data").expect("write");
        w.commit().expect("commit");

        assert_eq!(
            reader_calls.load(Ordering::SeqCst),
            0,
            "writer must not fire the reader hook"
        );
        assert_eq!(
            exists_calls.load(Ordering::SeqCst),
            0,
            "writer must not fire the exists hook"
        );

        assert!(cache.exists(&a, "h", "out").expect("exists"));
        assert_eq!(
            exists_calls.load(Ordering::SeqCst),
            1,
            "exists hook must fire on exists"
        );
        assert_eq!(
            reader_calls.load(Ordering::SeqCst),
            0,
            "exists must not fire the reader hook"
        );

        let sized = cache.reader(&a, "h", "out").expect("reader");
        drop(sized);
        assert_eq!(
            reader_calls.load(Ordering::SeqCst),
            1,
            "reader hook must fire on reader"
        );
        assert_eq!(
            exists_calls.load(Ordering::SeqCst),
            1,
            "reader must not re-fire the exists hook"
        );

        // Unset hook: call must still forward correctly and must not panic on
        // the no-op default.
        let entries = cache.list_target_entries(&a).expect("list_target_entries");
        assert_eq!(
            entries,
            vec!["h".to_string()],
            "call still forwards with no hook installed"
        );
    }
}
