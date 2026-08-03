[package]
name = "proto-gen"
version = "0.1.0"
edition = "2021"

[dependencies]
prost = "0.14"
prost-types = "0.14"
serde = { version = "1.0", features = ["derive"] }
pbjson = "0.9"

[lints.clippy]
all = "allow"

[features]
# @@protoc_insertion_point(features)
