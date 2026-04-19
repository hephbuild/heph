use std::collections::HashMap;
use crate::htpkg::PkgBuf;

#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Addr {
    pub package: PkgBuf,
    pub name: String,
    pub args: HashMap<String, String>,
}

impl Addr {
    pub fn format(&self) -> String {
        format!("//{}:{}", self.package, self.name)
    }
}
