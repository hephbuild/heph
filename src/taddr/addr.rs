use std::collections::HashMap;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TAddr {
    pub package: String,
    pub name: String,
    pub args: HashMap<String, String>,
}
