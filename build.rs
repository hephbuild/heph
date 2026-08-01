//! Weak-link libfuse on macOS so the binary launches without macFUSE installed.
//!
//! With the default `fuse-sandbox` feature, `fuser` links libfuse on macOS
//! (there is no pure-Rust mount path there). A normal link makes
//! `/usr/local/lib/libfuse.2.dylib` a hard dependency: dyld aborts the whole
//! process at launch if it is missing — before any config or `support_check`
//! runs. Emitting `-weak-lfuse` turns it into an `LC_LOAD_WEAK_DYLIB`: dyld
//! tolerates the dylib being absent and binds its symbols to null. Those
//! symbols are only ever called behind the runtime `support_check` gate (which
//! requires macFUSE present), so a null is never dereferenced.
//!
//! Linux FUSE is pure-Rust with no libfuse link, so nothing is emitted there.
//!
//! Also detects whether this build carries frame pointers, for `--pprof-cpu`
//! (see [`frame_pointers`]).
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    // Only when libfuse is actually linked (the only config that links it).
    let fuse = std::env::var_os("CARGO_FEATURE_FUSE_SANDBOX").is_some();
    if macos && fuse {
        println!("cargo::rustc-link-arg=-weak-lfuse");
    }
    frame_pointers();
}

/// Set `cfg(heph_frame_pointers)` when this build asks rustc to keep a frame
/// record in every function.
///
/// `--pprof-cpu` walks that chain from its `SIGPROF` handler (`src/pprof_dump.rs`
/// explains why it cannot use the unwinder). Without frame pointers the walk does
/// not crash — it reads whatever the register happened to hold and reports a
/// plausible-looking wrong stack. Nothing downstream can tell the difference, so
/// the condition is detected here, at the only place that can see the flags, and
/// `pprof_dump::start` refuses to profile without it rather than emitting
/// fiction.
///
/// `.cargo/config.toml` sets the flag for every build in this workspace. An
/// explicit `RUSTFLAGS` in the environment *replaces* that config (cargo does not
/// merge them), which is exactly the case this has to catch — and the reason for
/// a runtime refusal rather than a build failure: an unrelated
/// `RUSTFLAGS=-Ctarget-cpu=native` should still build heph, just not silently
/// turn its profiler into a random-stack generator.
fn frame_pointers() {
    println!("cargo::rustc-check-cfg=cfg(heph_frame_pointers)");
    println!("cargo::rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    println!("cargo::rerun-if-env-changed=RUSTFLAGS");
    // aarch64-apple-darwin keeps the frame record regardless of the flag — the
    // platform ABI reserves x29 for it — so the guarantee holds there even if a
    // build overrides RUSTFLAGS.
    let apple_arm = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64");
    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    if apple_arm || forces_frame_pointers(&flags) {
        println!("cargo::rustc-cfg=heph_frame_pointers");
    }
}

/// Whether `flags` (the `\u{1f}`-separated `CARGO_ENCODED_RUSTFLAGS`) turn
/// `force-frame-pointers` on.
///
/// The option reaches rustc as `-Cforce-frame-pointers=yes`, as a bare
/// `-Cforce-frame-pointers` (implicitly on), as `--codegen force-frame-pointers`
/// in two tokens, and with `=no` to turn it back off — and the last spelling
/// wins, so a later `=no` has to lose to nothing earlier.
fn forces_frame_pointers(flags: &str) -> bool {
    let mut on = false;
    let mut tokens = flags.split('\u{1f}').filter(|t| !t.is_empty()).peekable();
    while let Some(token) = tokens.next() {
        // `-C opt` and `--codegen opt` put the option in the *next* token.
        let opt = match token {
            "-C" | "--codegen" => match tokens.next() {
                Some(next) => next,
                None => break,
            },
            _ => token
                .strip_prefix("-C")
                .or_else(|| token.strip_prefix("--codegen="))
                .unwrap_or(""),
        };
        let Some(value) = opt.strip_prefix("force-frame-pointers") else {
            continue;
        };
        on = matches!(value, "" | "=yes" | "=y" | "=on" | "=true");
    }
    on
}
