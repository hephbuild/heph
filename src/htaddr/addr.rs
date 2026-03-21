use std::collections::HashMap;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Addr {
    pub package: String,
    pub name: String,
    pub args: HashMap<String, String>,
}
