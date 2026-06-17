//! rkyv zero-copy mirrors of the hot-path messages.
//!
//! Only the messages on the shm fast path are mirrored here (callback `result`
//! / `note_dep` + their payloads). The cold path reuses prost inside the shm
//! sample. Each mirror has `From`/`Into` conversions to the prost type in
//! [`crate::pb`]; the parity test (`tests/parity.rs`) round-trips proto -> rkyv
//! bytes -> proto and asserts equality, so the two representations cannot drift.
//!
//! `args` maps are carried as a sorted `Vec<(String, String)>` for a stable,
//! deterministic archived layout (rkyv map support is avoided on the hot path).

use crate::pb;

/// rkyv mirror of [`pb::Addr`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Addr {
    pub package: String,
    pub name: String,
    /// Sorted (key, value) pairs.
    pub args: Vec<(String, String)>,
}

/// rkyv mirror of [`pb::ArtifactHandle`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ArtifactHandle {
    pub handle_id: String,
    pub group: String,
    pub name: String,
    pub hashout: String,
    pub byte_size: u64,
    pub support: bool,
}

/// rkyv mirror of [`pb::ResultRequest`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ResultRequest {
    pub request_id: String,
    pub addr: Addr,
}

/// rkyv mirror of [`pb::ResultResponse`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ResultResponse {
    pub lease_id: String,
    pub artifacts: Vec<ArtifactHandle>,
}

/// rkyv mirror of [`pb::NoteDepRequest`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NoteDepRequest {
    pub request_id: String,
    pub parent: Addr,
    pub addr: Addr,
}

/// rkyv mirror of [`pb::NoteDepResponse`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NoteDepResponse {
    pub ok: bool,
    pub cycle: bool,
    pub message: String,
}

// ---- conversions ----

impl From<pb::Addr> for Addr {
    fn from(a: pb::Addr) -> Self {
        let mut args: Vec<(String, String)> = a.args.into_iter().collect();
        args.sort();
        Addr {
            package: a.package,
            name: a.name,
            args,
        }
    }
}

impl From<Addr> for pb::Addr {
    fn from(a: Addr) -> Self {
        pb::Addr {
            package: a.package,
            name: a.name,
            args: a.args.into_iter().collect(),
        }
    }
}

impl From<pb::ArtifactHandle> for ArtifactHandle {
    fn from(h: pb::ArtifactHandle) -> Self {
        ArtifactHandle {
            handle_id: h.handle_id,
            group: h.group,
            name: h.name,
            hashout: h.hashout,
            byte_size: h.byte_size,
            support: h.support,
        }
    }
}

impl From<ArtifactHandle> for pb::ArtifactHandle {
    fn from(h: ArtifactHandle) -> Self {
        pb::ArtifactHandle {
            handle_id: h.handle_id,
            group: h.group,
            name: h.name,
            hashout: h.hashout,
            byte_size: h.byte_size,
            support: h.support,
        }
    }
}

impl From<pb::ResultRequest> for ResultRequest {
    fn from(r: pb::ResultRequest) -> Self {
        ResultRequest {
            request_id: r.request_id,
            addr: r.addr.unwrap_or_default().into(),
        }
    }
}

impl From<ResultRequest> for pb::ResultRequest {
    fn from(r: ResultRequest) -> Self {
        pb::ResultRequest {
            request_id: r.request_id,
            addr: Some(r.addr.into()),
        }
    }
}

impl From<pb::ResultResponse> for ResultResponse {
    fn from(r: pb::ResultResponse) -> Self {
        ResultResponse {
            lease_id: r.lease_id,
            artifacts: r.artifacts.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ResultResponse> for pb::ResultResponse {
    fn from(r: ResultResponse) -> Self {
        pb::ResultResponse {
            lease_id: r.lease_id,
            artifacts: r.artifacts.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<pb::NoteDepRequest> for NoteDepRequest {
    fn from(r: pb::NoteDepRequest) -> Self {
        NoteDepRequest {
            request_id: r.request_id,
            parent: r.parent.unwrap_or_default().into(),
            addr: r.addr.unwrap_or_default().into(),
        }
    }
}

impl From<NoteDepRequest> for pb::NoteDepRequest {
    fn from(r: NoteDepRequest) -> Self {
        pb::NoteDepRequest {
            request_id: r.request_id,
            parent: Some(r.parent.into()),
            addr: Some(r.addr.into()),
        }
    }
}

impl From<pb::NoteDepResponse> for NoteDepResponse {
    fn from(r: pb::NoteDepResponse) -> Self {
        NoteDepResponse {
            ok: r.ok,
            cycle: r.cycle,
            message: r.message,
        }
    }
}

impl From<NoteDepResponse> for pb::NoteDepResponse {
    fn from(r: NoteDepResponse) -> Self {
        pb::NoteDepResponse {
            ok: r.ok,
            cycle: r.cycle,
            message: r.message,
        }
    }
}
