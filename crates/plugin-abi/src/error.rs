//! Typed transport error, carried as the wire `Error` frame's `kind`. Callers
//! match on the kind (a well-known code), never on the message string. The
//! host classifies an error into a kind by downcasting to the real engine error
//! types; the guest maps the kind back to a typed error for plugin code.

use crate::pb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireErrorKind {
    Other,
    Cancelled,
    NotFound,
    Cycle,
}

impl WireErrorKind {
    pub fn from_pb(kind: i32) -> Self {
        match pb::error::Kind::try_from(kind).unwrap_or(pb::error::Kind::Other) {
            pb::error::Kind::Cancelled => Self::Cancelled,
            pb::error::Kind::NotFound => Self::NotFound,
            pb::error::Kind::Cycle => Self::Cycle,
            pb::error::Kind::Other | pb::error::Kind::Unspecified => Self::Other,
        }
    }

    pub fn as_pb(self) -> pb::error::Kind {
        match self {
            Self::Cancelled => pb::error::Kind::Cancelled,
            Self::NotFound => pb::error::Kind::NotFound,
            Self::Cycle => pb::error::Kind::Cycle,
            Self::Other => pb::error::Kind::Other,
        }
    }
}

/// An error received over the transport, with its kind preserved.
#[derive(Debug, Clone)]
pub struct WireError {
    pub kind: WireErrorKind,
    pub message: String,
}

impl WireError {
    pub fn from_pb(e: pb::Error) -> Self {
        Self {
            kind: WireErrorKind::from_pb(e.kind),
            message: e.message,
        }
    }

    pub fn to_pb(&self) -> pb::Error {
        pb::Error {
            kind: self.as_pb_kind() as i32,
            message: self.message.clone(),
        }
    }

    fn as_pb_kind(&self) -> pb::error::Kind {
        self.kind.as_pb()
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WireError {}
