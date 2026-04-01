use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Addr {
    pub package: String,
    pub name: String,
    pub args: HashMap<String, String>,
}

impl Addr {
    pub fn format(&self) -> String {
        format!("//{}:{}", self.package, self.name)
    }
}
