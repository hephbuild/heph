use std::path::PathBuf;
use crate::{engine, pluginbuildfile, pluginexec};
use crate::engine::driver::sandbox::Sandbox;
use crate::engine::provider::{StaticProvider, TargetSpec};
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;

pub fn new_engine() -> anyhow::Result<std::sync::Arc<engine::Engine>> {
    let root = match engine::get_root() {
        Ok(r) => r,
        Err(inner) => anyhow::bail!("Error: {}", inner)
    };

    let mut e = engine::Engine::new(engine::Config{
        root: PathBuf::from(root),
    })?;

    e.register_provider(|_root| Box::new(StaticProvider {
        targets: vec![
            TargetSpec {
                addr: Addr{
                    package: PkgBuf::from("some"),
                    name: "t".to_string(),
                    args: Default::default(),
                },
                driver: "exec".to_string(),
                config: Default::default(),
                labels: vec![],
                transitive: Sandbox::default(),
            },
        ],
        packages: Default::default(),
    }))?;
    e.register_provider(|root| Box::new(pluginbuildfile::Provider{
        root: root.to_path_buf(),
        ..pluginbuildfile::Provider::default()
    }))?;
    e.register_driver(Box::new(pluginexec::Driver::new_exec()))?;
    e.register_driver(Box::new(pluginexec::Driver::new_bash()))?;

    Ok(std::sync::Arc::new(e))
}