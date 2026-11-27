//! Machine-readable output for heph commands: `--json`, `--fields`, `--jq`.
//!
//! A command opts in with two lines: a `#[derive(Record)]` on the type it
//! prints, and a flattened [`JsonArgs`] on its clap struct. A command that has
//! not opted in has none of the three flags, so clap rejects them with exit 2.
//!
//! ```
//! # use serde::Serialize;
//! #[derive(Serialize, output::Record)]
//! pub struct Row<'a> {
//!     /// The target's address.
//!     addr: &'a str,
//!     /// The target's input hash.
//!     #[record(built)]
//!     hashin: Option<String>,
//! }
//!
//! #[derive(clap::Args)]
//! pub struct Args {
//!     #[command(flatten)]
//!     out: output::JsonArgs<Row<'static>>,
//! }
//!
//! fn execute(args: &Args) -> anyhow::Result<()> {
//!     if !args.out.enabled() {
//!         // The command's human output, unchanged.
//!         return Ok(());
//!     }
//!     // Compute only what was selected (I4): the id is generated, not a string.
//!     let hashin = args.out.wants(Row::HASHIN).then(|| "abc".to_string());
//!     let rows = [Row { addr: "//a:b", hashin }];
//!     args.out.list_iter(rows.iter(), &output::never_cancelled)
//! }
//! ```
//!
//! # Declaring fields
//!
//! Every serialized field of a `Record` is a field `--fields` can name, under
//! its serialized name (`rename`/`rename_all` honoured; `flatten` refused). Its
//! doc comment is its description in `--help` and `--fields help`. Its tier is
//! the work it adds beyond what the command already does, declared with
//! `#[record(...)]`:
//!
//! | Attribute | Tier | In the default set |
//! |---|---|---|
//! | none (or `free`) | [`Tier::Free`] | yes |
//! | `opt_in` | [`Tier::Free`] | no |
//! | `resolved` | [`Tier::Resolved`]: a spec or def the command would not fetch | no |
//! | `built` | [`Tier::Built`]: inputs hashed, so dependencies build | no |
//!
//! A migrated command's default set is exactly the keys it printed before.
//! The command computes a non-default field only when [`JsonArgs::wants`] says
//! so; unselected fields are dropped by the writer either way, so leaving one
//! `None` when it is not wanted is enough. A shipped field is never renamed or
//! removed, never changes type, tier or default membership (I3).
//!
//! # Printing
//!
//! Under any of the three flags ([`JsonArgs::enabled`]) stdout carries the
//! document and nothing else (I1): keep the TUI off and print nothing else.
//! Then, once:
//!
//! * a record-shaped command calls [`JsonArgs::record`], or
//!   [`JsonArgs::null`] when it has nothing to report (never `[]`);
//! * a list-shaped command calls [`JsonArgs::list`] (a stream of
//!   `anyhow::Result<row>`, written as it is found), [`JsonArgs::list_iter`],
//!   or drives a [`ListWriter`] from [`JsonArgs::printer`] itself.
//!
//! A list that does not complete — an `Err` from the stream, a
//! [`ListWriter`] dropped without [`ListWriter::finish`] — is left without its
//! `]`, so `jq` rejects it instead of reading a short list as whole (D10). A
//! broken pipe stops the list and returns `Ok`: the command exits 0, and since
//! the stream is no longer polled, the walk stops ([`Flow::Stop`] tells a
//! [`ListWriter`] driver to cancel its request). Any other write error is an
//! error with context.
//!
//! A command that prints JSON either way and has no stable shape (D11:
//! `inspect spec`, `inspect def`, `tool config-schema`) flattens [`DocArgs`]
//! instead: `--json` and `--jq`, no `--fields`, no `Record`; it prints any
//! `Serialize` value with [`DocArgs::print`].
//!
//! An engine command hands its flags to the runner as [`OutputFlags`], so the
//! runner keeps the TUI off under any of them; the command cannot forget.
//!
//! `cancelled` is polled while a `--jq` filter runs (on its own thread, which
//! is abandoned, not joined, on cancellation): pass the request's
//! `ctoken().is_cancelled()`, or [`never_cancelled`] in a command with no
//! request.
//!
//! # Exit codes
//!
//! A command's own exit code is the same with or without the flags. A `--jq`
//! runtime error is exit 2 and a cancelled filter 130, through
//! [`exit_code`], which `main` consults first. `--fields help` prints the
//! field table and exits 0, through [`fields_help`], which `main` checks
//! right after parsing.

// `#[derive(Record)]` emits `::output::…` paths; this lets the crate's own
// tests use it.
extern crate self as output;

mod args;
#[cfg(doctest)]
mod derive_refusals;
mod doc;
mod error;
mod filter;
mod jq;
pub mod ser;
mod write;

use std::marker::PhantomData;

pub use args::{FieldSel, JsonArgs, fields_help};
pub use doc::{DocArgs, OutputFlags};
pub use error::{OutputError, exit_code};
pub use jq::Jq;
pub use output_derive::{Record, Shape};
pub use write::{Flow, ListWriter, Printer, Style};

/// The version of the output contract `heph version --json` reports as
/// `output_contract`. It moves only if I3 (a shipped field is never renamed,
/// removed or retyped) is ever broken.
///
/// 2: error reporting retyped `inspect failure`'s `addr`, `hashin` and
/// `failed_at` to nullable, and reshaped `run`: `blocked_by` and the
/// `blocked` outcome removed, `skipped` added, `addr` nullable, and `file`,
/// `root`, `more`, `path` and `selected` added.
pub const OUTPUT_CONTRACT: u32 = 2;

/// A type a command prints as JSON. Implemented by `#[derive(Record)]`.
pub trait Record {
    /// The type with every lifetime `'static`: what [`JsonArgs`] is
    /// instantiated with, and what ties a borrowed row to it.
    type Static: Record + 'static;

    /// Every serialized field, in declaration order.
    const FIELDS: &'static [Field];
}

impl<T: Record + ?Sized> Record for &T {
    type Static = T::Static;
    const FIELDS: &'static [Field] = T::FIELDS;
}

/// One serialized field of a [`Record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The key it serializes as.
    pub name: &'static str,
    /// Its doc comment.
    pub doc: &'static str,
    /// The work it adds beyond what the command already does.
    pub tier: Tier,
    /// Whether it is printed when `--fields` does not name it.
    pub default: bool,
    /// The JSON kind its type serializes as, pinned by the field snapshot:
    /// `string`, `number`, `bool`, `any`, `K?` (nullable), `[K]`, `{K}`
    /// (map), or `@Name` for a nested type the snapshot expands.
    pub kind: &'static str,
}

/// The work a field adds beyond what its command already does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    /// Already computed.
    Free,
    /// Needs a spec or def the command would not otherwise fetch.
    Resolved,
    /// Needs the target's inputs hashed, so its dependencies build.
    Built,
}

impl Tier {
    /// The name `--help` and `--fields help` print.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Free => "free",
            Tier::Resolved => "resolved",
            Tier::Built => "built",
        }
    }
}

/// A typed handle on one field of `R`, generated by `#[derive(Record)]` as an
/// upper-case associated constant (`Row::HASHIN`).
pub struct FieldId<R> {
    index: usize,
    _record: PhantomData<fn() -> R>,
}

impl<R> FieldId<R> {
    #[doc(hidden)]
    pub const fn __new(index: usize) -> Self {
        Self {
            index,
            _record: PhantomData,
        }
    }

    /// The field's position in `R::FIELDS`.
    pub fn index(self) -> usize {
        self.index
    }
}

impl<R> Clone for FieldId<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for FieldId<R> {}

impl<R> std::fmt::Debug for FieldId<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FieldId").field(&self.index).finish()
    }
}

/// Which fields of a record are printed: a bitmask over `FIELDS` indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    mask: u64,
}

impl Selection {
    /// The fields `FIELDS` marks as default.
    pub fn defaults(fields: &[Field]) -> Self {
        Self::matching(fields, |f| f.default)
    }

    /// Every field.
    pub fn all(fields: &[Field]) -> Self {
        Self::matching(fields, |_| true)
    }

    fn matching(fields: &[Field], keep: impl Fn(&Field) -> bool) -> Self {
        let mask = fields
            .iter()
            .enumerate()
            .filter(|(_, f)| keep(f))
            .fold(0, |mask, (i, _)| mask | bit(i));
        Self { mask }
    }

    /// Add the field at `index`.
    pub fn insert(&mut self, index: usize) {
        self.mask |= bit(index);
    }

    /// Whether the field at `index` is selected.
    pub fn contains(self, index: usize) -> bool {
        self.mask & bit(index) != 0
    }
}

/// The mask bit of field `index`; the derive refuses more than 64 fields.
fn bit(index: usize) -> u64 {
    1u64.checked_shl(u32::try_from(index).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

/// I8 for one value: every key `value` serializes (all fields selected) is in
/// `T::FIELDS`. Each `Record`'s unit test calls this on a populated value.
pub fn check_keys<T: Record + serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    // Unfiltered: the filtering serializer itself refuses an unlisted key.
    serde::Serialize::serialize(value, &mut serde_json::Serializer::new(&mut buf))?;
    let doc: serde_json::Value = serde_json::from_slice(&buf)?;
    let serde_json::Value::Object(map) = doc else {
        anyhow::bail!("a Record serializes as a JSON object, got {doc}");
    };
    let unknown: Vec<&String> = map
        .keys()
        .filter(|k| !T::FIELDS.iter().any(|f| f.name == k.as_str()))
        .collect();
    anyhow::ensure!(
        unknown.is_empty(),
        "keys not in the field table: {unknown:?}"
    );
    Ok(())
}

/// For a command with no request to cancel: nothing ever cancels it.
pub fn never_cancelled() -> bool {
    false
}

/// What a command polls to learn it was cancelled (Ctrl-C).
pub type Cancelled<'a> = &'a (dyn Fn() -> bool + Send + Sync);

#[cfg(test)]
pub(crate) mod fixtures {
    use serde::Serialize;

    /// A query-shaped row with one field of every tier.
    #[derive(Debug, Clone, Serialize, crate::Record)]
    pub(crate) struct Row {
        /// The target's address.
        pub addr: String,
        /// Labels declared on the target.
        pub labels: Vec<String>,
        /// The driver that runs it.
        #[record(opt_in)]
        pub driver: Option<String>,
        /// Its dependencies.
        #[record(resolved)]
        pub deps: Option<Vec<String>>,
        /// Its input hash.
        #[record(built)]
        pub hashin: Option<String>,
    }

    pub(crate) fn row(i: usize) -> Row {
        Row {
            addr: format!("//p:t{i}"),
            labels: vec!["test".into()],
            driver: Some("exec".into()),
            deps: Some(vec![format!("//p:d{i}")]),
            hashin: Some(format!("h{i}")),
        }
    }

    pub(crate) fn args(flags: &[&str]) -> crate::JsonArgs<Row> {
        crate::JsonArgs::try_parse_from(flags).expect("flags parse")
    }
}
