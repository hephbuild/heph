use crate::engine::Engine;
use crate::engine::packages::PackagesResult;
use crate::htaddr::Addr;
use crate::htmatcher;

pub struct QueryResult<'a> {
    e: &'a Engine,
    m: &'a htmatcher::Matcher,
    it: PackagesResult<'a>,
}

impl Iterator for QueryResult<'_> {
    type Item = Addr;

    fn next(&mut self) -> Option<Self::Item> {
        self.it.next().map(|p| Addr {
            package: "".to_string(),
            name: p.to_string(),
            args: Default::default(),
        })
    }
}

impl Engine {
    pub fn query<'a>(&'a self, m: &'a htmatcher::Matcher) -> QueryResult<'a> {
        // for pkg in self.packages(&htmatcher::Matcher::Package(addr.package)) {
        //
        // }

        QueryResult{e: self, m, it: self.packages(m)}
    }
}
