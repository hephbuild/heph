//! Stable-ABI boundary for in-process dynamically-loaded heph plugins.
//!
//! Goal: load a plugin as a cdylib and call its surface with ZERO serialization
//! on the hot path — the high-volume `ProviderExecutor` callbacks (result /
//! note_dep / query, ~22k per `test //...`) cross as direct stabby vtable calls
//! over native-ish types, recovering the in-process floor (~3.1s vs ~7s
//! out-of-process) while keeping the plugin a separately-built, hot-swappable,
//! ABI-stable artifact.
//!
//! Scoping: only the hot callback path is native. The cold, low-volume
//! Provider/Driver methods (config/list/get/parse/run, ~2k calls ≈ 130ms total)
//! cross as prost bytes — cheap, and lenient via protobuf — so the gnarly
//! `TargetSpec`/`TargetDef`/`raw_def` types need no stabby mirror.
//!
//! `Addr` crosses as its canonical `//pkg:name` string (parsed at the seam;
//! ~22k parses ≈ tens of ms). Result artifacts cross as eagerly-read bytes
//! (plugin-go reads a tiny `package.bin`), wrapped back into a local `Content`.
//!
//! This crate is the **host** side plus the shared ABI contract ([`abi`]). The
//! guest side (serving an `hplugin` Provider/Driver behind the ABI) lives in
//! `plugin-sdk`, which depends on this crate for [`abi`].

#![allow(
    clippy::expl_impl_clone_on_copy,
    reason = "stabby's #[stabby] macro derives Clone on the zero-sized Copy vtable marker types it generates; the lint fires on macro output we don't control"
)]

pub mod abi;

#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "host")]
pub mod load_stable;
