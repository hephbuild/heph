//! The fold key: which recorded paths are other casings or normalisations of
//! a walked one (Decision 13).
//!
//! NFC, then Unicode **simple** case folding (`CaseFolding.txt` statuses C and
//! S), never a locale lowering: `ß` stays `ß`, `İ` stays `İ`. A name that is not
//! UTF-8 is folded by ASCII lowercase of its bytes. Over-folding only adds
//! candidates, which the inode check rejects; a key collision is caught the
//! same way. Computed without allocating, into a streaming hash.

use unicode_normalization::UnicodeNormalization as _;
use xxhash_rust::xxh3::Xxh3;

/// The 64-bit fold key of a root-relative path: the hash of [`fold_bytes`].
pub fn fold_hash(path: &[u8]) -> u64 {
    let mut h = Xxh3::new();
    fold_into(path, |b| h.update(b));
    h.digest()
}

/// The folded form of a path, as the index stores it to load one directory
/// with every other casing of it ([`super::store::Store::load_under`]).
///
/// Folding a `/`-joined path folds each side of the `/` on its own (NFC
/// composes nothing across `/`, and case folding is per char), so a path
/// under directory `d` folds to something under `fold_bytes(d)` — when both
/// are UTF-8. A path that is not UTF-8 folds by ASCII lowercase of its bytes,
/// so it is under [`ascii_fold`]`(d)` instead.
pub fn fold_bytes(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len());
    fold_into(path, |b| out.extend_from_slice(b));
    out
}

/// ASCII lowercase of `path`'s bytes: how a path that is not UTF-8 folds.
pub fn ascii_fold(path: &[u8]) -> Vec<u8> {
    path.to_ascii_lowercase()
}

fn fold_into(path: &[u8], mut sink: impl FnMut(&[u8])) {
    match std::str::from_utf8(path) {
        Ok(s) if !s.is_ascii() => {
            let mut buf = [0u8; 4];
            for c in s.nfc() {
                sink(simple_fold(c).encode_utf8(&mut buf).as_bytes());
            }
        }
        // ASCII is already NFC, and its simple folding is ASCII lowercase:
        // the same bytes either way.
        _ => {
            let mut buf = [0u8; 256];
            for chunk in path.chunks(buf.len()) {
                let out = buf.get_mut(..chunk.len()).unwrap_or_default();
                for (o, b) in out.iter_mut().zip(chunk) {
                    *o = b.to_ascii_lowercase();
                }
                sink(out);
            }
        }
    }
}

/// Unicode simple case folding of one char.
fn simple_fold(c: char) -> char {
    unicode_case_mapping::case_folded(c)
        .and_then(|n| char::from_u32(n.get()))
        .unwrap_or(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn casings_share_a_key() {
        assert_eq!(fold_hash(b"gen/a.go"), fold_hash(b"Gen/A.GO"));
        assert_ne!(fold_hash(b"gen/a.go"), fold_hash(b"gen/b.go"));
    }

    #[test]
    fn nfc_and_nfd_share_a_key() {
        let nfc = "\u{e9}.go";
        let nfd = "e\u{301}.go";
        assert_ne!(nfc.as_bytes(), nfd.as_bytes());
        assert_eq!(fold_hash(nfc.as_bytes()), fold_hash(nfd.as_bytes()));
        assert_eq!(fold_hash(nfc.as_bytes()), fold_hash("\u{c9}.go".as_bytes()));
    }

    #[test]
    fn simple_folding_without_locale() {
        // Full folding would make these equal (ß → ss); simple folding does not.
        assert_ne!(
            fold_hash("straße.go".as_bytes()),
            fold_hash("STRASSE.go".as_bytes())
        );
        // İ has only full and Turkic foldings.
        assert_ne!(fold_hash("İ.go".as_bytes()), fold_hash(b"i.go"));
        // Simple folding where lowercase is not enough: final sigma, Kelvin.
        assert_eq!(fold_hash("ς".as_bytes()), fold_hash("Σ".as_bytes()));
        assert_eq!(fold_hash("\u{212a}".as_bytes()), fold_hash(b"k"));
    }

    #[test]
    fn non_utf8_names_fold_by_ascii_lowercase_of_the_bytes() {
        assert_eq!(fold_hash(b"A\xff.go"), fold_hash(b"a\xff.go"));
        assert_ne!(fold_hash(b"a\xff.go"), fold_hash(b"a\xfe.go"));
    }

    /// What `load_under` relies on: a path under `d` folds to a path under
    /// `fold_bytes(d)` (both UTF-8) or under `ascii_fold(d)` (path not UTF-8),
    /// and the key is the hash of the folded bytes.
    #[test]
    fn a_path_under_a_dir_folds_under_the_folded_dir() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"Gen", b"Gen/A.go"),
            ("Caf\u{e9}".as_bytes(), "Caf\u{e9}/x\u{301}.go".as_bytes()),
            // NFD dir: the combining mark stays on its side of the `/`.
            ("Cafe\u{301}".as_bytes(), "Cafe\u{301}/\u{301}x".as_bytes()),
            ("\u{212a}".as_bytes(), "\u{212a}/\u{3a3}".as_bytes()),
        ];
        for (dir, path) in cases {
            let mut under = fold_bytes(dir);
            under.push(b'/');
            assert!(fold_bytes(path).starts_with(&under), "{path:?}");
            assert_eq!(
                fold_hash(path),
                xxhash_rust::xxh3::xxh3_64(&fold_bytes(path))
            );
        }
        // Not UTF-8 under a non-ASCII dir: under the dir's ASCII fold.
        let path = ["Caf\u{e9}/A".as_bytes(), b"\xff"].concat();
        let mut under = ascii_fold("Caf\u{e9}".as_bytes());
        under.push(b'/');
        assert!(fold_bytes(&path).starts_with(&under));
    }

    #[test]
    fn long_ascii_paths_hash_every_chunk() {
        let a = "x/".repeat(300) + "a";
        let b = "x/".repeat(300) + "b";
        assert_ne!(fold_hash(a.as_bytes()), fold_hash(b.as_bytes()));
        assert_eq!(
            fold_hash(a.as_bytes()),
            fold_hash(a.to_uppercase().as_bytes())
        );
    }
}
