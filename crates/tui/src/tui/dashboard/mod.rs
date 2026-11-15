//! The full-screen dashboard's model and list view (unit 5b). Not wired to
//! the terminal yet: Tab still opens the box.
//!
//! - [`model::Model`] folds the event stream into one fixed-size record per
//!   target, its address interned once, kept in per-state buckets in
//!   activity order.
//! - [`header::header_lines`] draws the title, gauge, tab and rule rows.
//! - [`list::list_lines`] draws the active tab's rows. Its LAST LINE column
//!   reads [`list::LastLines`], filled off the render task by
//!   [`list::read_tails`] over a [`list::LogReader`].

mod bucket;
pub mod header;
pub mod intern;
pub mod list;
pub mod model;

pub use header::{Levels, Title, header_lines};
pub use intern::Id;
pub use list::{
    LastLines, ListInputs, ListState, LogReader, TailRequest, TailUpdate, WaitWords, Waits,
    list_lines, read_tails,
};
pub use model::{Gauge, Model, Record, RowState, Tab, TabCounts};
