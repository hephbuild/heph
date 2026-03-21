use std::io;
use crate::htaddr::Addr;

pub trait LocalCache {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Read>, anyhow::Error>;
    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>, anyhow::Error>;
    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool, anyhow::Error>;
    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<(), anyhow::Error>;
}
