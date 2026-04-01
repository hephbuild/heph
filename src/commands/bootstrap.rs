use std::path::PathBuf;
use crate::engine;

pub fn new_engine() -> anyhow::Result<std::sync::Arc<engine::Engine>> {
    let cwd = engine::get_cwd();
    let root = match engine::get_root() {
        Ok(r) => r,
        Err(inner) => anyhow::bail!("Error: {}", inner)
    };

    engine::Engine::new(engine::Config{
        root: PathBuf::from(root),
    })
}