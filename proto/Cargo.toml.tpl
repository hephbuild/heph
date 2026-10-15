[package]
name = "proto-gen"
version = "0.1.0"
edition = "2021"

# `protoc-gen-buffa-packaging` emits the module tree as `mod.rs`; `gen` copies
# this file next to it.
[lib]
path = "src/mod.rs"

[dependencies]
# The workspace pins it exactly, in step with `buffaVersion` in devenv.nix: the
# plugin and the runtime are released together.
buffa = { workspace = true }

[lints.clippy]
all = "allow"
