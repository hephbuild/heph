//! I10: the machine sampler allocates nothing per sample on its own thread.
//!
//! Its own test binary: it installs a counting `#[global_allocator]`, and the
//! lib's test binary already has one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use tui::tui::session::sampler::{self, Reading, Sample};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn count(bytes: usize) {
    ALLOCS.with(|c| c.set(c.get() + 1));
    BYTES.with(|c| c.set(c.get() + bytes as u64));
}

// SAFETY: every method forwards to `System`, which upholds the `GlobalAlloc`
// contract; the wrapper adds no assumption of its own. The counters are
// const-initialised thread-local `Cell`s: they never allocate and have no
// destructor, so bumping them cannot re-enter the allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: `layout` is forwarded unchanged from the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: `layout` is forwarded unchanged from the caller.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        // SAFETY: `ptr`, `layout` and `new_size` are forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` are forwarded unchanged from the caller.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn snapshot() -> (u64, u64) {
    (ALLOCS.with(Cell::get), BYTES.with(Cell::get))
}

#[test]
fn sampler_allocates_nothing_per_sample() {
    // What goes through the channel stays small and fixed-size.
    assert!(
        size_of::<Sample>() <= 256,
        "Sample is {} bytes",
        size_of::<Sample>()
    );
    assert!(
        size_of::<Reading>() <= 256,
        "Reading is {} bytes",
        size_of::<Reading>()
    );

    let (out, readings) = sampler::channel();
    // A slow consumer, so the loop's try_send both delivers and finds the
    // channel full.
    let drain = std::thread::spawn(move || {
        let (mut delivered, mut dropped) = (0u64, 0u64);
        for reading in readings {
            if let Reading::Sample(s) = reading {
                delivered += 1;
                dropped += u64::from(s.dropped);
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        (delivered, dropped)
    });

    // The loop runs on this thread, so the thread-local counters are its.
    // Wake 1 comes before the first tick, wake 2 after it, and wake 102 after
    // 100 more.
    let mut wakes = 0u32;
    let mut after_first = (0, 0);
    let mut after_last = (0, 0);
    sampler::run_host_loop(Duration::from_millis(1), &out, || {
        wakes += 1;
        match wakes {
            2 => after_first = snapshot(),
            102 => {
                after_last = snapshot();
                return true;
            }
            _ => {}
        }
        false
    });
    drop(out);
    let (delivered, dropped) = drain.join().expect("drain");

    assert_eq!(wakes, 102, "the loop ran 101 ticks");
    assert_eq!(
        (after_last.0 - after_first.0, after_last.1 - after_first.1),
        (0, 0),
        "allocations and bytes over 100 ticks on the sampler thread"
    );
    assert!(delivered > 0, "some samples were delivered");
    assert!(dropped > 0, "some samples met a full channel");
}
