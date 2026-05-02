use crate::{engine, pluginbuildfile, pluginexec};

pub fn new_engine() -> anyhow::Result<std::sync::Arc<engine::Engine>> {
    let root = match engine::get_root() {
        Ok(r) => r,
        Err(inner) => anyhow::bail!("Error: {}", inner)
    };

    let mut e = engine::Engine::new(engine::Config{
        root,
    })?;

    e.register_provider(|root| Box::new(pluginbuildfile::Provider{
        root: root.to_path_buf(),
        ..pluginbuildfile::Provider::default()
    }))?;
    e.register_managed_driver(Box::new(pluginexec::Driver::new_exec()))?;
    e.register_managed_driver(Box::new(pluginexec::Driver::new_bash()))?;

    Ok(std::sync::Arc::new(e))
}