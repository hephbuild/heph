use crate::engine::config_file;
use crate::{
    engine, pluginbuildfile, pluginexec, pluginfs, plugingo, pluginhostbin, pluginnix,
    plugintextfile,
};

pub fn new_engine() -> anyhow::Result<std::sync::Arc<engine::Engine>> {
    let root = match engine::get_root() {
        Ok(r) => r,
        Err(inner) => anyhow::bail!("Error: {}", inner),
    };

    let cfg_path = root.join(".hephconfig2");
    let file = config_file::load(&cfg_path)?;

    let home_dir = file
        .home_dir
        .as_ref()
        .map(|p| root.join(p))
        .unwrap_or_else(|| root.join(".heph3"));

    let mut e = engine::Engine::new(engine::Config {
        root: root.clone(),
        home_dir: home_dir.clone(),
        parallelism: None,
    })?;

    // Auto-registered built-ins (no options).
    e.register_provider(|_| Box::new(pluginfs::Provider))?;
    e.register_driver(Box::new(pluginfs::Driver))?;
    e.register_provider(|_| Box::new(pluginhostbin::Provider))?;
    e.register_driver(Box::new(pluginhostbin::Driver))?;
    e.register_driver(Box::new(plugintextfile::Driver))?;
    e.register_managed_driver(Box::new(pluginnix::Driver::new(
        home_dir.join("nix-driver"),
    )))?;

    // Opt-in factories — instantiated by `apply_config` if listed in the YAML.
    e.register_provider_factory("buildfile", |root, opts| {
        Ok(Box::new(pluginbuildfile::Provider::from_options(
            root.to_path_buf(),
            opts,
        )?))
    })?;
    e.register_provider_factory("go", |root, opts| {
        Ok(Box::new(plugingo::Provider::from_options(
            root.to_path_buf(),
            opts,
        )?))
    })?;

    e.register_managed_driver_factory("exec", |opts| {
        Ok(Box::new(pluginexec::Driver::from_options_exec(opts)?))
    })?;
    e.register_managed_driver_factory("bash", |opts| {
        Ok(Box::new(pluginexec::Driver::from_options_bash(opts)?))
    })?;
    e.register_managed_driver_factory("go_golist", |opts| {
        config_file::deny_unknown("go_golist driver", opts, &[])?;
        Ok(Box::new(plugingo::GoGolistDriver::new("//@heph/bin:go")))
    })?;
    e.register_managed_driver_factory("go_embed", |opts| {
        config_file::deny_unknown("go_embed driver", opts, &[])?;
        Ok(Box::new(plugingo::GoEmbedDriver))
    })?;
    e.register_managed_driver_factory("go_testmain", |opts| {
        config_file::deny_unknown("go_testmain driver", opts, &[])?;
        Ok(Box::new(plugingo::GoTestmainDriver))
    })?;

    e.apply_config(&file.providers, &file.drivers)?;

    Ok(std::sync::Arc::new(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_engine_from_yaml(yaml: &str) -> anyhow::Result<(tempfile::TempDir, engine::Engine)> {
        let file: config_file::ConfigFile = serde_yaml::from_str(yaml)?;
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let home_dir = file
            .home_dir
            .as_ref()
            .map(|p| root.join(p))
            .unwrap_or_else(|| root.join(".heph3"));
        let mut e = engine::Engine::new(engine::Config {
            root,
            home_dir: home_dir.clone(),
            parallelism: None,
        })?;

        e.register_provider(|_| Box::new(pluginfs::Provider))?;
        e.register_driver(Box::new(pluginfs::Driver))?;
        e.register_provider(|_| Box::new(pluginhostbin::Provider))?;
        e.register_driver(Box::new(pluginhostbin::Driver))?;
        e.register_driver(Box::new(plugintextfile::Driver))?;
        e.register_managed_driver(Box::new(pluginnix::Driver::new(
            home_dir.join("nix-driver"),
        )))?;

        e.register_provider_factory("buildfile", |root, opts| {
            Ok(Box::new(pluginbuildfile::Provider::from_options(
                root.to_path_buf(),
                opts,
            )?))
        })?;
        e.register_managed_driver_factory("exec", |opts| {
            Ok(Box::new(pluginexec::Driver::from_options_exec(opts)?))
        })?;
        e.register_managed_driver_factory("bash", |opts| {
            Ok(Box::new(pluginexec::Driver::from_options_bash(opts)?))
        })?;

        e.apply_config(&file.providers, &file.drivers)?;
        Ok((dir, e))
    }

    #[test]
    fn applies_listed_providers_and_drivers() {
        let yaml = r#"
providers:
  - name: buildfile
    options:
      patterns: [BUILD]
drivers:
  - name: exec
  - name: bash
"#;
        let (_dir, e) = build_engine_from_yaml(yaml).expect("engine");
        assert!(e.providers_by_name.contains_key("buildfile"));
        assert!(e.drivers_by_name.contains_key("exec"));
        assert!(e.drivers_by_name.contains_key("bash"));
        assert!(e.providers_by_name.contains_key("fs"));
    }

    #[test]
    fn unknown_provider_errors() {
        let yaml = r#"
providers:
  - name: nope
"#;
        let err = build_engine_from_yaml(yaml).err().expect("must error");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn unknown_driver_errors() {
        let yaml = r#"
drivers:
  - name: ghost
"#;
        let err = build_engine_from_yaml(yaml).err().expect("must error");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn empty_config_only_loads_builtins() {
        let (_dir, e) = build_engine_from_yaml("").expect("engine");
        assert!(!e.providers_by_name.contains_key("buildfile"));
        assert!(!e.drivers_by_name.contains_key("exec"));
        assert!(e.providers_by_name.contains_key("fs"));
        assert!(e.drivers_by_name.contains_key("fs"));
    }
}
