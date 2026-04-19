use crate::htaddr::Addr;

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

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum MatchResult {
    Yes,
    No,
    Shrug,
}

impl Matcher {
    pub fn match_addr(&self, addr: &Addr) -> MatchResult {
        match self {
            Matcher::Addr(a) => {
                if a == addr {
                    MatchResult::Yes
                } else {
                    MatchResult::No
                }
            }
            Matcher::Package(pkg) => {
                if &addr.package == pkg {
                    MatchResult::Yes
                } else {
                    MatchResult::No
                }
            }
            Matcher::PackagePrefix(prefix) => {
                if addr.package.starts_with(prefix.as_str()) {
                    MatchResult::Yes
                } else {
                    MatchResult::No
                }
            }
            Matcher::Label(_) => MatchResult::Shrug,
            Matcher::Or(matchers) => {
                let mut has_shrug = false;
                for m in matchers {
                    match m.match_addr(addr) {
                        MatchResult::Yes => return MatchResult::Yes,
                        MatchResult::Shrug => has_shrug = true,
                        MatchResult::No => {}
                    }
                }
                if has_shrug {
                    MatchResult::Shrug
                } else {
                    MatchResult::No
                }
            }
            Matcher::And(matchers) => {
                let mut has_shrug = false;
                for m in matchers {
                    match m.match_addr(addr) {
                        MatchResult::No => return MatchResult::No,
                        MatchResult::Shrug => has_shrug = true,
                        MatchResult::Yes => {}
                    }
                }
                if has_shrug {
                    MatchResult::Shrug
                } else {
                    MatchResult::Yes
                }
            }
            Matcher::Not(m) => match m.match_addr(addr) {
                MatchResult::Yes => MatchResult::No,
                MatchResult::No => MatchResult::Yes,
                MatchResult::Shrug => MatchResult::Shrug,
            },
        }
    }
}
