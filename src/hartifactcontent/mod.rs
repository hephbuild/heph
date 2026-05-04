pub mod tar;
pub mod unpack;

use std::io;
use std::path::PathBuf;

pub struct WalkEntry {
    pub path: PathBuf,
    pub data: Box<dyn io::Read>,
    pub x: bool,
}

#[derive(Clone, Copy)]
pub enum Type {
    Tar,
    Cpio,
}

pub trait Content: Send + Sync {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>>;
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item=anyhow::Result<WalkEntry>> + '_>>;
    fn hashout(&self) -> anyhow::Result<String>;
}
