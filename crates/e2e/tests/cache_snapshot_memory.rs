//! A multi-hundred-MB scratch slot goes into a cache snapshot in bounded
//! memory: packed to a temp file, then streamed into the generation, never
//! held whole.
//!
//! A separate test binary because it installs a process-wide counting
//! allocator: the export packs and copies on tokio's blocking threads, so a
//! thread-local count would see none of it, and another test sharing the
//! process would pollute a global one.

use heph::engine::scratch_remote::ScratchDestination;
use heph::engine::{Config, Engine};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicIsize, Ordering};

/// Heap bytes the process holds now, and the most it has held since the last
/// [`reset_peak`].
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

struct CountingAlloc;

fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

fn live_add(delta: isize) {
    // Relaxed is enough: each `fetch_add` returns LIVE's value in its single
    // modification order, so the `fetch_max` of those values is its true
    // maximum; nothing else is published through these counters.
    let live = LIVE.fetch_add(delta, Ordering::Relaxed).wrapping_add(delta);
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System`, which upholds the `GlobalAlloc`
// contract; the wrapper adds no assumptions of its own. The counters are
// atomics in statics: bumping them never allocates, so it cannot re-enter the
// allocator.
unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged from the caller.
        let p = unsafe { std::alloc::System.alloc(layout) };
        if !p.is_null() {
            live_add(signed(layout.size()));
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged from the caller.
        let p = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !p.is_null() {
            live_add(signed(layout.size()));
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr`, `layout` and `new_size` are forwarded unchanged.
        let p = unsafe { std::alloc::System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            live_add(signed(new_size).wrapping_sub(signed(layout.size())));
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: `ptr` and `layout` are forwarded unchanged from the caller.
        unsafe { std::alloc::System.dealloc(ptr, layout) };
        live_add(-signed(layout.size()));
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

/// Start a new peak at what the process holds now; returns that baseline.
fn reset_peak() -> isize {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    live
}

const MIB: usize = 1 << 20;
/// The slot: four 64 MiB files.
const FILES: usize = 4;
const FILE_MIB: usize = 64;
/// What the export may hold above its baseline. A slot buffered whole would
/// need its full 256 MiB at least once.
const BOUND: usize = 32 * MIB;

/// Write `FILES` incompressible files under `dir`, through one reused 1 MiB
/// buffer, so the archive is as large as the slot and the fixture itself
/// stays out of the peak.
fn write_slot(dir: &Path) {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut buf = vec![0u8; MIB];
    for i in 0..FILES {
        let sub = dir.join(format!("d{i}"));
        std::fs::create_dir_all(&sub).expect("mkdir slot subdir");
        let mut f = std::fs::File::create(sub.join("blob.bin")).expect("create slot file");
        for _ in 0..FILE_MIB {
            for chunk in buf.as_chunks_mut::<8>().0 {
                // xorshift64: fast, and gzip can't shrink it.
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *chunk = state.to_le_bytes();
            }
            f.write_all(&buf).expect("write slot file");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_hundred_mb_slot_is_exported_in_bounded_memory() {
    let root = tempfile::tempdir().expect("workspace");
    let slot_dir = tempfile::tempdir().expect("slot");
    let generation = tempfile::tempdir().expect("generation");
    write_slot(slot_dir.path());

    let engine = Engine::new(Config::for_tests(root.path())).expect("engine");

    // The count sees an allocation, so a small peak below means a small heap.
    let probe = reset_peak();
    drop(std::hint::black_box(vec![1u8; 4 * MIB]));
    assert!(
        PEAK.load(Ordering::Relaxed).wrapping_sub(probe) >= signed(4 * MIB),
        "the counting allocator missed a 4 MiB allocation"
    );

    let baseline = reset_peak();
    let published = engine
        .scratch_publish(
            "slot",
            "main",
            slot_dir.path(),
            None,
            "test",
            ScratchDestination::Dir {
                root: generation.path(),
                prev: None,
            },
        )
        .await
        .expect("export the slot");
    let held = PEAK.load(Ordering::Relaxed).wrapping_sub(baseline);

    let slot_bytes = FILES * FILE_MIB * MIB;
    let written = usize::try_from(published.bytes).expect("bytes fit usize");
    assert!(
        written >= slot_bytes,
        "the archive holds the whole incompressible slot: {written} < {slot_bytes}"
    );
    // The entry really is in the generation, at that size.
    let archives: Vec<_> = walk_files(generation.path())
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".tar.gz"))
        .collect();
    assert_eq!(archives.len(), 1, "{archives:?}");
    let on_disk = std::fs::metadata(&archives[0]).expect("archive").len();
    assert_eq!(on_disk, published.bytes, "{}", archives[0].display());

    assert!(
        usize::try_from(held).is_ok_and(|h| h < BOUND),
        "exporting a {} MiB slot held {} MiB of heap at its peak; the bound is {} MiB",
        slot_bytes / MIB,
        held / signed(MIB),
        BOUND / MIB,
    );
}

fn walk_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read_dir") {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}
