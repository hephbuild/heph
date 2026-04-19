use crate::htaddr::Addr;
use crate::htpkg::Pkg;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Matcher {
    Addr(Addr),
    Label(Addr),
    Package(String),
    PackagePrefix(String),
    Or(Vec<Matcher>),
    And(Vec<Matcher>),
    Not(Box<Matcher>),
}

impl Matcher {
    pub fn match_addr(&self, addr: &Addr) -> bool {
        match self {
            Matcher::Addr(a) => addr == a,
            Matcher::Label(_) => false,
            Matcher::Package(p) => &addr.package == p,
            Matcher::PackagePrefix(prefix) => {
                Pkg::new(&addr.package).has_prefix(Pkg::new(prefix))
            }
            Matcher::Or(matchers) => matchers.iter().any(|m| m.match_addr(addr)),
            Matcher::And(matchers) => matchers.iter().all(|m| m.match_addr(addr)),
            Matcher::Not(m) => !m.match_addr(addr),
        }
    }
}
