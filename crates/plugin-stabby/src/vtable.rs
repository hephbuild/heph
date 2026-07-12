//! Safe construction of stabby dyn handles.
//!
//! On stable Rust, stabby cannot hold a dyn-object's vtable in a `const` (see
//! `stabby_abi::vtable::IConstConstructor`), so every `Box<T>` → `Dyn…`
//! conversion looks that type's vtable up in a process-global registry
//! (`stabby_abi::vtable::VTABLES`, an `AtomicArc`-rooted btree) and inserts it on
//! first use. The registry's read path loads the root pointer and only THEN
//! clones the root `Arc` — nothing keeps the pointee alive across that window, so
//! a concurrent first-time insert (which compare-exchanges a new root in and
//! drops the old one) frees the node the reader is about to clone. The reader
//! walks freed memory and can come back with a null vtable slice, aborting the
//! process in `NonNull::new_unchecked` (`stabby-abi-72.1.8/src/vtable/mod.rs:170`,
//! "unsafe precondition(s) violated") or segfault outright. 72.1.8 is the latest
//! release, so there is no version to upgrade to.
//!
//! Registry lookups only race against registry MUTATION, and every mutation in
//! this process comes from a dyn construction in this workspace — so serializing
//! our constructions behind one process-global lock closes the window: while a
//! first-use insert is swapping the root, no other thread is reading it.
//!
//! This is off the hot path. It costs one uncontended lock per handle
//! construction (a per-RPC boxed future, a per-call executor handle); every
//! *method call* on an existing handle — `note_dep`, `next`, `read_chunk` — is a
//! direct vtable dispatch that never touches the registry.
//!
//! The host binary and each loaded cdylib statically link their own copy of
//! stabby, hence their own `VTABLES` — and their own copy of this lock, which
//! guards exactly the registry its side constructs into.

use std::sync::Mutex;

/// Guards `stabby_abi::vtable::VTABLES` against concurrent insert-vs-lookup.
static VTABLE_REGISTRY: Mutex<()> = Mutex::new(());

/// Build a stabby dyn handle (`stabby::boxed::Box<T>` → `Dyn…`) with the vtable
/// registry locked.
///
/// EVERY stabby dyn construction in the workspace must go through this instead of
/// a bare `.into()` — a single unguarded construction can free the registry root
/// under another thread and abort the process.
pub fn dynify<T, D: From<T>>(value: T) -> D {
    // The lock only spans stabby's registry insert — never user code — so a poison
    // flag carries no broken invariant of ours: take the guard and continue.
    let _guard = VTABLE_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    D::from(value)
}

#[cfg(test)]
mod tests {
    use super::dynify;
    use crate::abi::{DynRead, StableRead, StableReadDyn};
    use stabby::vec::Vec as SVec;

    /// A distinct `StableRead` implementor per `N` — each monomorphization is a
    /// distinct source type, so it gets its OWN vtable and its own first-use
    /// insert into the registry. One type would be inserted once and every later
    /// construction would take the read-only path; many types keep the registry
    /// mutating while other threads read it, which is the racy window.
    struct Reader<const N: usize>;

    impl<const N: usize> StableRead for Reader<N> {
        extern "C" fn read_chunk(&self) -> SVec<u8> {
            SVec::from([N as u8].as_slice())
        }
    }

    /// Construct one handle per type and call through each, so a vtable that came
    /// back corrupted shows up as a wrong answer rather than passing silently.
    macro_rules! build_all {
        ($($n:literal),* $(,)?) => {
            $({
                let handle: DynRead = dynify(stabby::boxed::Box::new(Reader::<$n>));
                let got = handle.read_chunk();
                assert_eq!(got.as_slice(), [$n as u8], "vtable dispatched to the wrong impl");
            })*
        };
    }

    /// Many threads registering many first-use vtables at once must not corrupt
    /// the registry. Pre-fix this aborts the process (non-unwinding panic in
    /// `NonNull::new_unchecked`) once a reader clones a root another thread just
    /// freed; a plain `.into()` here is enough to trip it.
    #[test]
    fn concurrent_first_use_vtable_registration_is_sound() {
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    build_all!(
                        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
                        21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39,
                        40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58,
                        59, 60, 61, 62, 63,
                    );
                });
            }
        });
    }
}
