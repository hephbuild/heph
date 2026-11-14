//! Session ids: 10 Crockford base32 characters that sort by start time.
//!
//! The id is opaque to a user — a future daemon runs many sessions in one
//! process, so a pid cannot name one — but it is built so that sorting ids sorts
//! sessions: the top 42 bits are the start time in unix milliseconds (good until
//! the year 2109), the low 8 bits are random. 50 bits, 5 per character.
//!
//! Uniqueness does not rest on those 8 bits: the owner creates its directory with
//! `create_dir` (never `create_dir_all`) and draws a new id when the name is
//! taken, so two sessions started in the same millisecond still get two ids.
//!
//! The encoding is hand-rolled (it is twenty lines) and lowercase. Decoding
//! accepts upper case and Crockford's aliases (`i`/`l` → `1`, `o` → `0`), so an
//! id read off a screen and retyped still resolves.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Crockford's base32 alphabet, lowercase. Ascending in ASCII, so a
/// fixed-width id sorts lexicographically exactly as its value sorts.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Characters in an id.
pub const ID_LEN: usize = 10;

/// Bits of the id holding the start time.
const TIME_BITS: u32 = 42;
/// Bits of the id holding randomness.
const RANDOM_BITS: u32 = 8;

/// A session's id. See the module docs for the layout.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(u64);

impl SessionId {
    /// The id for a session started at `start_unix_ms`, with `random` as its
    /// low bits. Milliseconds past the 42-bit range wrap; that is year 2109.
    pub fn new(start_unix_ms: u64, random: u8) -> Self {
        let time = start_unix_ms & ((1u64 << TIME_BITS) - 1);
        Self((time << RANDOM_BITS) | u64::from(random))
    }

    /// A fresh id for a session starting now, its random byte drawn from a v4
    /// uuid (already in the dependency tree; no second RNG).
    pub fn generate(start_unix_ms: u64) -> Self {
        Self::new(start_unix_ms, random_byte())
    }

    /// The start time this id encodes, in unix milliseconds.
    pub fn started_unix_ms(self) -> u64 {
        self.0 >> RANDOM_BITS
    }

    /// The id's text, `ID_LEN` lowercase characters.
    pub fn encode(self) -> String {
        (0..ID_LEN)
            .rev()
            .map(|i| {
                // `& 31` keeps the index inside the 32-entry alphabet.
                let digit = (self.0 >> (i * 5)) & 31;
                ALPHABET
                    .get(usize::try_from(digit).unwrap_or(0))
                    .copied()
                    .map_or('0', char::from)
            })
            .collect()
    }

    /// Parse an id's text. Exactly [`ID_LEN`] characters of the alphabet, any
    /// case, with Crockford's aliases.
    pub fn parse(s: &str) -> Result<Self, InvalidSessionId> {
        if s.chars().count() != ID_LEN {
            return Err(InvalidSessionId(s.to_owned()));
        }
        s.chars()
            .try_fold(0u64, |acc, c| {
                decode_char(c).map(|d| (acc << 5) | u64::from(d))
            })
            .map(Self)
            .ok_or_else(|| InvalidSessionId(s.to_owned()))
    }
}

/// The value of one id character, or `None` for a character outside the
/// alphabet. Exposed for prefix and suffix matching, which accept the same
/// spellings a full id does.
pub fn decode_char(c: char) -> Option<u8> {
    let c = match c.to_ascii_lowercase() {
        'i' | 'l' => '1',
        'o' => '0',
        c => c,
    };
    ALPHABET
        .iter()
        .position(|&a| char::from(a) == c)
        .and_then(|p| u8::try_from(p).ok())
}

/// Normalise a (partial) id as typed — aliases resolved, lowercased — so a
/// prefix or suffix compares against [`SessionId::encode`] text. `None` when a
/// character is outside the alphabet.
pub fn normalize(s: &str) -> Option<String> {
    s.chars()
        .map(|c| {
            decode_char(c)
                .and_then(|d| ALPHABET.get(usize::from(d)))
                .map(|&a| char::from(a))
        })
        .collect()
}

pub(crate) fn random_byte() -> u8 {
    uuid::Uuid::new_v4()
        .as_bytes()
        .first()
        .copied()
        .unwrap_or(0)
}

/// Text that is not a session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSessionId(pub String);

impl fmt::Display for InvalidSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a session id ({ID_LEN} characters of 0-9 and a-z without i, l, o, u)",
            self.0
        )
    }
}

impl std::error::Error for InvalidSessionId {}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({})", self.encode())
    }
}

impl FromStr for SessionId {
    type Err = InvalidSessionId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for SessionId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.encode())
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_ten_lowercase_characters_and_round_trips() {
        let id = SessionId::new(1_763_000_000_000, 0xab);
        let text = id.encode();
        assert_eq!(text.len(), ID_LEN);
        assert!(
            text.chars().all(|c| ALPHABET.contains(&(c as u8))),
            "{text}"
        );
        assert_eq!(SessionId::parse(&text), Ok(id));
        assert_eq!(id.started_unix_ms(), 1_763_000_000_000);
        // Upper case and the aliases decode to the same id.
        assert_eq!(SessionId::parse(&text.to_uppercase()), Ok(id));
        let zero = SessionId::new(0, 0).encode();
        assert_eq!(zero, "0000000000");
        assert_eq!(SessionId::parse("oooooooooo"), Ok(SessionId::new(0, 0)));
        assert_eq!(
            SessionId::parse("000000000l"),
            Ok(SessionId::new(0, 1)),
            "l is an alias of 1"
        );
    }

    #[test]
    fn ids_sort_by_start_time_as_text() {
        // Adjacent milliseconds, a random byte that would invert them if it
        // were the high bits, and a jump across a character boundary.
        let times = [1_000u64, 1_001, 1_032, 1_763_000_000_000, 1_763_000_000_001];
        let ids: Vec<SessionId> = times
            .iter()
            .zip([255u8, 0, 7, 255, 0])
            .map(|(&t, r)| SessionId::new(t, r))
            .collect();
        let mut by_text: Vec<String> = ids.iter().map(|i| i.encode()).collect();
        let expected = by_text.clone();
        by_text.sort();
        assert_eq!(by_text, expected, "text order is start order");
        let mut by_value = ids.clone();
        by_value.sort();
        assert_eq!(by_value, ids);
    }

    #[test]
    fn rejects_wrong_length_and_foreign_characters() {
        for bad in [
            "",
            "abc",
            "00000000000",
            "000000000u",
            "00000000-0",
            "ééééééééé",
        ] {
            assert!(SessionId::parse(bad).is_err(), "{bad:?} must not parse");
        }
        assert_eq!(normalize("K7Q2"), Some("k7q2".to_owned()));
        assert_eq!(normalize("k7u"), None);
    }

    #[test]
    fn serde_is_the_text() {
        let id = SessionId::new(42, 1);
        let json = serde_json::to_string(&id).expect("encode");
        assert_eq!(json, format!("\"{id}\""));
        let back: SessionId = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, id);
        assert!(serde_json::from_str::<SessionId>("\"nope\"").is_err());
    }
}
