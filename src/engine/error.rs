use crate::htaddr::Addr;
use std::fmt;

#[derive(Debug, Clone)]
pub struct TargetNotFoundError {
    pub addr: Addr,
}

impl fmt::Display for TargetNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "target not found: {}", self.addr.format())
    }
}

impl std::error::Error for TargetNotFoundError {}

#[derive(Debug, Clone)]
pub struct CycleError {
    pub from: Addr,
    pub to: Addr,
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cyclic dependency detected: {} → {}",
            self.from.format(),
            self.to.format()
        )
    }
}

impl std::error::Error for CycleError {}
