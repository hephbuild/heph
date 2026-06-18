//! The managed-driver contract shared by the exec-family plugins (exec/go/nix):
//! the `ManagedDriver` trait, its request/response shapes, the OS-copy sandbox
//! runner (`ManagedDriverOs`), and the shared output/source-map helpers. This
//! crate is deliberately **fuse-free** so plugins that merely implement
//! `ManagedDriver` don't link `fuser`/libfuse. The FUSE backend and the
//! `ManagedDriverBridge` that routes between OS and FUSE live in
//! `heph-driver-bridge`, which the engine wires via `Engine::new_managed_driver`.
pub mod driver_managed;
pub mod driver_managed_os;
pub mod fuseconfig;
