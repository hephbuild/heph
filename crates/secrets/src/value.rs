//! A minted credential: the one type in the tree allowed to hold a live value.
//!
//! Concentrating that in one place is what makes the rest of the design
//! checkable. A `Debug` that prints a token turns every `tracing` call in every
//! driver into a leak, so [`SecretValue`] has a hand-written `Debug` and no
//! `Display` at all — reaching the bytes takes [`SecretValue::expose`], which
//! is a word a reviewer can grep for.

use crate::expiry::Expiry;
use std::collections::BTreeMap;
use std::fmt;

/// A value that must not be printed.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the bytes. Named to be conspicuous in review and in `grep`.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Never the value. The length is safe and is what a diagnostic actually wants
/// ("did the helper return anything at all?").
impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretValue(<{} bytes>)", self.0.len())
    }
}

impl From<String> for SecretValue {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// What a provider returns: one or more named fields plus when they die.
///
/// A map rather than a scalar because half the protocols are inherently
/// multi-field — `credential_process` returns three, `docker_credential`
/// returns two — and flattening them into "the value" would make the
/// `aws_profile` shape guess at which was which.
#[derive(Clone, Debug)]
pub struct Credential {
    /// Field name → value. Single-valued protocols use [`Credential::PRIMARY`].
    pub fields: BTreeMap<String, SecretValue>,
    pub expiry: Expiry,
}

impl Credential {
    /// The conventional field name for a single-valued credential, and the one
    /// `"$."` in an `env` map resolves to.
    pub const PRIMARY: &'static str = "token";

    pub fn single(value: impl Into<String>, expiry: Expiry) -> Self {
        Self {
            fields: BTreeMap::from([(Self::PRIMARY.to_string(), SecretValue::new(value))]),
            expiry,
        }
    }

    pub fn get(&self, field: &str) -> Option<&SecretValue> {
        self.fields.get(field)
    }

    /// Resolve one `env`-map pointer against this credential.
    ///
    /// `"$."` is the whole primary value; `"$.token"` names a field. Kept to
    /// exactly these two forms on purpose — a descriptor is close enough to a
    /// cache key that a real expression language is the wrong thing to put in
    /// it, and the two forms cover every protocol's output.
    pub fn resolve_pointer(&self, pointer: &str) -> anyhow::Result<&SecretValue> {
        let field = normalize_pointer(pointer)?;
        self.get(&field).ok_or_else(|| {
            let known = self.fields.keys().cloned().collect::<Vec<_>>().join(", ");
            anyhow::anyhow!(
                "the credential has no field {field:?}. It returned: [{known}]. \
                 Check the pointer against the protocol's response shape."
            )
        })
    }

    /// Every live value, for registering with the redactor.
    pub fn values(&self) -> impl Iterator<Item = &SecretValue> {
        self.fields.values()
    }
}

/// Reduce an `env` pointer to the field it names.
///
/// `"$."`, `"$"` and `"$.token"` all mean the primary field, so all three must
/// hash the same. They did not: the pointer is serialized verbatim into
/// `secret.json`, so the three spellings produced three cache keys and editing
/// one to another was a full rebuild of every consumer for a no-op change.
///
/// Exposed so the driver can normalize at declaration time, the same way
/// `scope` and `shape` are sorted before they can reach a key.
pub fn normalize_pointer(pointer: &str) -> anyhow::Result<String> {
    Ok(match pointer {
        "$." | "$" => Credential::PRIMARY.to_string(),
        p => p
            .strip_prefix("$.")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid env pointer {p:?}: expected \"$.\" for the whole value, or \
                     \"$.<field>\" for one field"
                )
            })?
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expiry::{Expiry, ExpirySource};
    use std::time::SystemTime;

    fn exp() -> Expiry {
        Expiry {
            at: SystemTime::UNIX_EPOCH,
            source: ExpirySource::Default,
        }
    }

    /// The single most important property of this type: a stray `{:?}` in any
    /// driver must not print a token.
    #[test]
    fn debug_never_prints_the_value() {
        let v = SecretValue::new("ghs_16C7e42F292c6912E7710c838347Ae178B4a");
        let shown = format!("{v:?}");
        assert!(!shown.contains("ghs_"), "{shown}");
        assert_eq!(shown, "SecretValue(<40 bytes>)");

        // ...and through a Credential, which is what actually gets logged.
        let c = Credential::single("ghs_16C7e42F292c6912E7710c838347Ae178B4a", exp());
        let shown = format!("{c:?}");
        assert!(!shown.contains("ghs_"), "{shown}");
    }

    #[test]
    fn the_bare_pointer_is_the_primary_field() {
        let c = Credential::single("abc", exp());
        assert_eq!(c.resolve_pointer("$.").expect("resolves").expose(), "abc");
        assert_eq!(c.resolve_pointer("$").expect("resolves").expose(), "abc");
        assert_eq!(
            c.resolve_pointer("$.token").expect("resolves").expose(),
            "abc"
        );
    }

    #[test]
    fn an_unknown_field_lists_what_the_protocol_actually_returned() {
        let c = Credential {
            fields: BTreeMap::from([
                ("AccessKeyId".to_string(), SecretValue::new("ASIA…")),
                ("SessionToken".to_string(), SecretValue::new("tok")),
            ]),
            expiry: exp(),
        };
        let err = c.resolve_pointer("$.token").expect_err("no such field");
        let msg = err.to_string();
        assert!(msg.contains("AccessKeyId"), "{msg}");
        assert!(msg.contains("SessionToken"), "{msg}");
        // The message names fields, never values.
        assert!(!msg.contains("ASIA"), "{msg}");
    }

    /// Three spellings of one field must be one cache key.
    #[test]
    fn pointer_spellings_normalize_to_one_field() {
        for spelling in ["$.", "$", "$.token"] {
            assert_eq!(
                normalize_pointer(spelling).expect("normalizes"),
                Credential::PRIMARY
            );
        }
        assert_eq!(
            normalize_pointer("$.AccessKeyId").expect("normalizes"),
            "AccessKeyId"
        );
    }

    #[test]
    fn a_malformed_pointer_is_rejected_with_the_two_legal_forms() {
        let c = Credential::single("abc", exp());
        let err = c.resolve_pointer("token").expect_err("no $ prefix");
        assert!(err.to_string().contains("$.<field>"), "{err}");
    }
}
