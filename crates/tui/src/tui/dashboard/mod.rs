//! The full-screen dashboard's model and list view (unit 5b). Not wired to
//! the terminal yet: Tab still opens the box.
//!
//! - [`model::Model`] folds the event stream into one fixed-size record per
//!   target, its address interned once, kept in per-state buckets in
//!   activity order.
//! - [`header::header_lines`] draws the title, gauge, tab and rule rows.
//! - [`frame::dashboard_frame`] puts the header, the list and the key-hint
//!   row into one frame.
//! - [`list::list_lines`] draws the active tab's rows. Its LAST LINE column
//!   reads [`list::LastLines`], filled off the render task by
//!   [`list::read_tails`] over a [`list::LogReader`].

pub mod approval;
mod bucket;
pub(crate) mod chrome;
pub mod detail;
pub mod feed;
pub mod frame;
pub mod header;
pub mod intern;
pub mod list;
pub mod logpane;
pub mod model;
pub mod tails;
pub mod target;
#[cfg(test)]
mod tests;

pub use feed::{
    FeedStep, FileKind, LogFeed, LogOpened, OutputFile, OutputGroup, OutputsFound, TargetReader,
};
pub use frame::{DASHBOARD_HINTS, dashboard_frame};
pub use header::{Levels, Title, Verdict, header_lines};
pub use intern::Id;
pub use list::{
    LastLines, ListHit, ListInputs, ListState, LogReader, TailRequest, TailUpdate, WaitWords,
    Waits, list_lines, read_tails,
};
pub use model::{Gauge, Model, Record, RowState, Tab, TabCounts};
pub use tails::TailReader;
pub use target::{TargetClick, TargetInputs, TargetScreen, TargetTab, WHEEL_ROWS};
