//! On macOS every binary is linked by ld-prime or lld, never Apple's old ld64.
//!
//! ld64 truncates a `__unwind_info` entry's offset into `__eh_frame` to 24
//! bits, and a debug test binary's `__eh_frame` is past 16 MiB. libunwind then
//! parses whatever lies at the wrapped offset, and a panic or an anyhow
//! backtrace crashes the process with `malformed DW_CFA_def_cfa_register
//! DWARF unwind, reg too big` — in whichever binary the layout happens to put
//! bad bytes there. `.cargo/config.toml` has the whole story.
//!
//! This binary is linked with the same flags as every other one, so it asks
//! its own `LC_BUILD_VERSION` which linker built it. ld64 and ld-prime both
//! record themselves as `ld`; the version tells them apart (954 vs 1000+).

#![cfg(target_os = "macos")]

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_BUILD_VERSION: u32 = 0x32;
const TOOL_LD: u32 = 3;
const TOOL_LLD: u32 = 4;
/// The first ld-prime release. A tool version is `major << 16 | minor << 8 | patch`.
const LD_PRIME_MAJOR: u32 = 1000;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let word = bytes.get(at..at + 4).expect("Mach-O truncated");
    u32::from_le_bytes(word.try_into().expect("4 bytes"))
}

/// The `(tool, version)` pairs in a 64-bit little-endian Mach-O's `LC_BUILD_VERSION`.
fn build_tools(bytes: &[u8]) -> Vec<(u32, u32)> {
    assert_eq!(u32_at(bytes, 0), MH_MAGIC_64, "not a 64-bit Mach-O");
    let ncmds = u32_at(bytes, 16);
    let mut at = 32; // sizeof(mach_header_64)
    for _ in 0..ncmds {
        let (cmd, size) = (u32_at(bytes, at), u32_at(bytes, at + 4) as usize);
        if cmd == LC_BUILD_VERSION {
            // cmd, cmdsize, platform, minos, sdk, ntools, then (tool, version) pairs.
            let ntools = u32_at(bytes, at + 20) as usize;
            return (0..ntools)
                .map(|i| {
                    let pair = at + 24 + i * 8;
                    (u32_at(bytes, pair), u32_at(bytes, pair + 4))
                })
                .collect();
        }
        at += size;
    }
    Vec::new()
}

/// Whether the linker that wrote `tools` addresses `__eh_frame` past 16 MiB correctly.
fn linker_is_sound(tools: &[(u32, u32)]) -> bool {
    let linkers: Vec<_> = tools
        .iter()
        .filter(|(t, _)| matches!(*t, TOOL_LD | TOOL_LLD))
        .collect();
    !linkers.is_empty()
        && linkers
            .iter()
            .all(|&&(tool, version)| tool == TOOL_LLD || version >> 16 >= LD_PRIME_MAJOR)
}

#[test]
fn this_binary_is_linked_by_ld_prime_or_lld() {
    let exe = std::env::current_exe().expect("current_exe");
    let bytes = std::fs::read(&exe).expect("read the test binary");
    let tools = build_tools(&bytes);
    let shown: Vec<_> = tools
        .iter()
        .map(|(t, v)| format!("tool {t} v{}.{}", v >> 16, (v >> 8) & 0xff))
        .collect();
    assert!(
        linker_is_sound(&tools),
        "{} was linked by neither ld-prime nor lld (LC_BUILD_VERSION: {shown:?}; tool 3 is ld, \
         ld64 below v{LD_PRIME_MAJOR}, ld-prime from it; tool 4 is lld). ld64 truncates unwind \
         offsets past 16 MiB of `__eh_frame`, and libunwind then crashes the process. \
         `.cargo/config.toml` sets `-fuse-ld=heph` for aarch64-apple-darwin; a `RUSTFLAGS` in the \
         environment replaces it.",
        exe.display(),
    );
}

#[test]
fn ld64_is_refused_and_ld_prime_and_lld_are_accepted() {
    let v = |major: u32, minor: u32| major << 16 | minor << 8;
    assert!(!linker_is_sound(&[(TOOL_LD, v(954, 16))]), "ld64");
    assert!(linker_is_sound(&[(TOOL_LD, v(1230, 1))]), "ld-prime");
    assert!(linker_is_sound(&[(TOOL_LLD, v(21, 1))]), "lld");
    assert!(!linker_is_sound(&[]), "no linker recorded");
}
