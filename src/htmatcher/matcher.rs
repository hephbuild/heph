use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Matcher {
    Addr(Addr),
    Label(Addr),
    Package(PkgBuf),
    PackagePrefix(PkgBuf),
    Or(Vec<Matcher>),
    And(Vec<Matcher>),
    Not(Box<Matcher>),
}
