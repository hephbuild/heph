//! Host platform identification with a unified naming convention.
//!
//! Every ecosystem names operating systems and architectures differently
//! (Rust: `macos`/`x86_64`/`aarch64`; Go/OCI/Docker: `darwin`/`amd64`/`arm64`).
//! This module exposes the host's os/arch in both the canonical Go/OCI
//! convention (the common-denominator naming) and the raw Rust convention,
//! so callers do not each reinvent the mapping.

/// Host operating system in canonical (Go/OCI) naming, e.g. `linux`, `darwin`.
pub fn os() -> &'static str {
    rust_os_to_canonical(std::env::consts::OS)
}

/// Host architecture in canonical (Go/OCI) naming, e.g. `amd64`, `arm64`.
pub fn arch() -> &'static str {
    rust_arch_to_canonical(std::env::consts::ARCH)
}

/// Host operating system as Rust reports it (`std::env::consts::OS`).
pub fn os_raw() -> &'static str {
    std::env::consts::OS
}

/// Host architecture as Rust reports it (`std::env::consts::ARCH`).
pub fn arch_raw() -> &'static str {
    std::env::consts::ARCH
}

/// Map a Rust os name to the canonical (Go/OCI) convention. Unknown names pass through.
pub fn rust_os_to_canonical(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other,
    }
}

/// Map a Rust arch name to the canonical (Go/OCI) convention. Unknown names pass through.
pub fn rust_arch_to_canonical(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arch_mapping() {
        assert_eq!(rust_arch_to_canonical("x86_64"), "amd64");
        assert_eq!(rust_arch_to_canonical("aarch64"), "arm64");
        assert_eq!(rust_arch_to_canonical("x86"), "386");
        assert_eq!(rust_arch_to_canonical("arm"), "arm");
        assert_eq!(rust_arch_to_canonical("riscv64"), "riscv64");
    }

    #[test]
    fn test_os_mapping() {
        assert_eq!(rust_os_to_canonical("macos"), "darwin");
        assert_eq!(rust_os_to_canonical("linux"), "linux");
        assert_eq!(rust_os_to_canonical("windows"), "windows");
    }

    #[test]
    fn test_host_values_non_empty() {
        assert!(!os().is_empty());
        assert!(!arch().is_empty());
        assert!(!os_raw().is_empty());
        assert!(!arch_raw().is_empty());
    }
}
