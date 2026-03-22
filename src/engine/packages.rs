use crate::engine::Engine;
use crate::htmatcher;

pub struct PackagesResult<'a> {
    items: Vec<&'a str>,
    index: usize,
}

impl<'a> Iterator for PackagesResult<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.items.len() {
            let item = self.items[self.index];
            self.index += 1;
            Some(item)
        } else {
            None
        }
    }
}

impl Engine {
    pub fn packages<'a>(&'a self, _m: &'a htmatcher::Matcher) -> PackagesResult<'a> {
        PackagesResult {
            items: vec!["1", "2"],
            index: 0,
        }
    }
}