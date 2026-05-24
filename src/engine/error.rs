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

/// Aggregates multiple errors from a concurrent fanout when `RequestState::fail_fast`
/// is disabled. Each inner error is rendered with anyhow's alternate `{:#}` so the
/// full context chain shows up.
#[derive(Debug)]
pub struct MultiError(pub Vec<anyhow::Error>);

impl fmt::Display for MultiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} errors:", self.0.len())?;
        for (i, e) in self.0.iter().enumerate() {
            writeln!(f, "  [{}] {:#}", i, e)?;
        }
        Ok(())
    }
}

impl std::error::Error for MultiError {}
