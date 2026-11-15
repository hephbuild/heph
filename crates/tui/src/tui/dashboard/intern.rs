//! Each address the dashboard sees, stored once and named by a `u32` id.

use std::hash::{BuildHasher, RandomState};

use hashbrown::HashTable;

/// An address's id: its index in the interner, and in the record table.
pub type Id = u32;

/// Addresses, each stored once in one arena, named by [`Id`].
///
/// The table holds ids only and hashes them through the arena, so the bytes
/// of an address exist once: no `String` per set, no key copy in a map.
#[derive(Debug, Default)]
pub struct Interner {
    /// Every address, back to back.
    arena: String,
    /// `ends[id]`: where address `id` ends in `arena`; it starts where
    /// `id - 1` ends.
    ends: Vec<usize>,
    table: HashTable<Id>,
    hasher: RandomState,
}

impl Interner {
    /// `addr`'s id, and whether it was new.
    pub fn intern(&mut self, addr: &str) -> (Id, bool) {
        let hash = self.hasher.hash_one(addr);
        let Self {
            arena,
            ends,
            table,
            hasher,
        } = self;
        if let Some(&id) = table.find(hash, |&id| slice(arena, ends, id) == addr) {
            return (id, false);
        }
        // Past `u32::MAX` addresses the table would alias ids; a build of
        // four billion targets is out of reach of everything else first.
        let id = Id::try_from(ends.len()).unwrap_or(Id::MAX);
        arena.push_str(addr);
        ends.push(arena.len());
        table.insert_unique(hash, id, |&id| hasher.hash_one(slice(arena, ends, id)));
        (id, true)
    }

    /// `addr`'s id, if it was interned.
    pub fn get(&self, addr: &str) -> Option<Id> {
        let hash = self.hasher.hash_one(addr);
        self.table
            .find(hash, |&id| slice(&self.arena, &self.ends, id) == addr)
            .copied()
    }

    /// The address `id` names; empty for an id never handed out.
    pub fn addr(&self, id: Id) -> &str {
        slice(&self.arena, &self.ends, id)
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
}

fn slice<'a>(arena: &'a str, ends: &[usize], id: Id) -> &'a str {
    let i = id as usize;
    let Some(&end) = ends.get(i) else {
        return "";
    };
    let start = match i.checked_sub(1) {
        Some(prev) => ends.get(prev).copied().unwrap_or(0),
        None => 0,
    };
    arena.get(start..end).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_each_address_once() {
        let mut names = Interner::default();
        let (a, new_a) = names.intern("//p:a");
        let (b, _) = names.intern("//p:ab");
        let (a2, new_a2) = names.intern("//p:a");
        assert!(new_a && !new_a2);
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(names.addr(b), "//p:ab");
        assert_eq!(names.get("//p:é"), None);
        assert_eq!(names.intern("//p:é").0, 2);
        assert_eq!(names.addr(2), "//p:é");
        assert_eq!(names.len(), 3);
        assert_eq!(names.addr(99), "");
    }
}
