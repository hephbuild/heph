//! What `#[derive(Record)]` refuses at compile time, as `compile_fail`
//! doctests.
//!
//! Each refusal follows a twin that compiles and differs from it only in the
//! line named, so a `compile_fail` cannot pass for an unrelated reason (a
//! missing import, a typo in the fixture). The refusals' messages are
//! asserted by `output-derive`'s unit tests.
//!
//! These were trybuild cases. trybuild copies the workspace's `[profile]`
//! into the manifest it generates through `serde_json`, and `starlark`
//! enables `serde_json/arbitrary_precision` workspace-wide, which turns every
//! integer in it into a table cargo rejects.
//!
//! # `#[serde(flatten)]`
//!
//! A flattened field's keys would not be in the field table.
//!
//! ```
//! #[derive(serde::Serialize)]
//! struct Inner {
//!     a: u8,
//! }
//!
//! #[derive(serde::Serialize, output::Record)]
//! struct Outer {
//!     inner: Inner,
//! }
//! # fn main() {}
//! ```
//!
//! ```compile_fail
//! #[derive(serde::Serialize)]
//! struct Inner {
//!     a: u8,
//! }
//!
//! #[derive(serde::Serialize, output::Record)]
//! struct Outer {
//!     #[serde(flatten)]
//!     inner: Inner,
//! }
//! # fn main() {}
//! ```
//!
//! # A misspelt tier
//!
//! ```
//! #[derive(serde::Serialize, output::Record)]
//! struct Row {
//!     #[record(built)]
//!     hashin: Option<String>,
//! }
//! # fn main() {}
//! ```
//!
//! ```compile_fail
//! #[derive(serde::Serialize, output::Record)]
//! struct Row {
//!     #[record(bulit)]
//!     hashin: Option<String>,
//! }
//! # fn main() {}
//! ```
//!
//! # A name `--fields` reserves
//!
//! ```
//! #[derive(serde::Serialize, output::Record)]
//! struct Row {
//!     #[serde(rename = "everything")]
//!     everything: u8,
//! }
//! # fn main() {}
//! ```
//!
//! ```compile_fail
//! #[derive(serde::Serialize, output::Record)]
//! struct Row {
//!     #[serde(rename = "all")]
//!     everything: u8,
//! }
//! # fn main() {}
//! ```
//!
//! # Not a struct with named fields
//!
//! ```
//! #[derive(serde::Serialize, output::Record)]
//! struct Row {
//!     a: u8,
//! }
//! # fn main() {}
//! ```
//!
//! ```compile_fail
//! #[derive(serde::Serialize, output::Record)]
//! enum Row {
//!     A,
//! }
//! # fn main() {}
//! ```
//!
//! # Another record's field id
//!
//! `JsonArgs::<A>::wants` takes only `A`'s ids.
//!
//! ```
//! #[derive(serde::Serialize, output::Record)]
//! struct A {
//!     x: u8,
//! }
//!
//! #[derive(serde::Serialize, output::Record)]
//! struct B {
//!     y: u8,
//! }
//!
//! fn main() {
//!     let args = output::JsonArgs::<A>::default();
//!     let _ = args.wants(A::X);
//! }
//! ```
//!
//! ```compile_fail,E0308
//! #[derive(serde::Serialize, output::Record)]
//! struct A {
//!     x: u8,
//! }
//!
//! #[derive(serde::Serialize, output::Record)]
//! struct B {
//!     y: u8,
//! }
//!
//! fn main() {
//!     let args = output::JsonArgs::<A>::default();
//!     let _ = args.wants(B::Y);
//! }
//! ```
