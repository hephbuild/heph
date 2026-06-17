//! Parity test: proto -> rkyv mirror -> rkyv bytes -> (zero-copy access +
//! deserialize) -> proto must round-trip exactly. This catches drift between
//! the prost schema and the hand-written rkyv hot-message mirrors: if a field
//! is added/removed on one side, the conversion stops compiling or the
//! round-trip stops matching.

use plugin_abi::{pb, shm_types};
use std::collections::HashMap;

fn sample_addr(pkg: &str, name: &str) -> pb::Addr {
    let mut args = HashMap::new();
    args.insert("goos".to_string(), "linux".to_string());
    args.insert("goarch".to_string(), "amd64".to_string());
    pb::Addr {
        package: pkg.to_string(),
        name: name.to_string(),
        args,
    }
}

/// proto -> mirror -> rkyv bytes -> checked zero-copy access + deserialize ->
/// mirror -> proto == original. Run on concrete types so the archived type's
/// `CheckBytes` bound (from the `Archive` derive) resolves.
macro_rules! roundtrip {
    ($proto:ty, $mirror:ty, $value:expr) => {{
        let original: $proto = $value;
        let mirror: $mirror = original.clone().into();
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&mirror).expect("rkyv serialize");
        // zero-copy access must validate
        let archived =
            rkyv::access::<<$mirror as rkyv::Archive>::Archived, rkyv::rancor::Error>(&bytes)
                .expect("rkyv access");
        let back: $mirror =
            rkyv::deserialize::<$mirror, rkyv::rancor::Error>(archived).expect("rkyv deserialize");
        let proto_back: $proto = back.into();
        assert_eq!(original, proto_back);
    }};
}

#[test]
fn result_request_parity() {
    roundtrip!(
        pb::ResultRequest,
        shm_types::ResultRequest,
        pb::ResultRequest {
            request_id: "req-1".to_string(),
            addr: Some(sample_addr("//foo/bar", "lib")),
        }
    );
}

#[test]
fn result_response_parity() {
    roundtrip!(
        pb::ResultResponse,
        shm_types::ResultResponse,
        pb::ResultResponse {
            lease_id: "lease-7".to_string(),
            artifacts: vec![
                pb::ArtifactHandle {
                    handle_id: "h1".to_string(),
                    group: "out".to_string(),
                    name: "package.bin".to_string(),
                    hashout: "abc123".to_string(),
                    byte_size: 4096,
                    support: false,
                },
                pb::ArtifactHandle {
                    handle_id: "h2".to_string(),
                    group: "log".to_string(),
                    name: "stderr".to_string(),
                    hashout: String::new(),
                    byte_size: 0,
                    support: true,
                },
            ],
        }
    );
}

#[test]
fn note_dep_request_parity() {
    roundtrip!(
        pb::NoteDepRequest,
        shm_types::NoteDepRequest,
        pb::NoteDepRequest {
            request_id: "req-2".to_string(),
            parent: Some(sample_addr("//a", "x")),
            addr: Some(sample_addr("//b", "y")),
        }
    );
}

#[test]
fn note_dep_response_parity() {
    roundtrip!(
        pb::NoteDepResponse,
        shm_types::NoteDepResponse,
        pb::NoteDepResponse {
            ok: false,
            cycle: true,
            message: "cycle: //a:x -> //b:y -> //a:x".to_string(),
        }
    );
}
