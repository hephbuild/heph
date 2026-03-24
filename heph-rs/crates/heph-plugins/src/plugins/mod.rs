//! Built-in plugins

pub mod exec;
pub mod buildfile;
pub mod fs;

pub use exec::ExecPlugin;
pub use buildfile::BuildFilePlugin;
pub use fs::FsPlugin;
