use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;

/// Label carried by the `go_compile_src` group so it can be selected by query
/// (e.g. `label(go_compile_src)`).
pub const GO_COMPILE_SRC_LABEL: &str = "go_compile_src";

/// Build the `go_compile_src` group: a transparent aggregate of the Go source
/// files the package's library compile consumes — i.e. exactly the default (`""`)
/// dep group `build_lib` feeds to `go tool compile`. `build_lib` depends on this
/// group instead of re-listing the sources, so the two never drift. `src_addrs`
/// are already-formatted `@heph/fs` file target addresses.
pub fn build_spec(addr: Addr, src_addrs: &[String]) -> TargetSpec {
    let deps: Vec<Value> = src_addrs.iter().cloned().map(Value::String).collect();

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("deps".to_string(), Value::List(deps));

    TargetSpec {
        addr,
        driver: hbuiltins::plugingroup::DRIVER_NAME.to_string(),
        config,
        labels: vec![GO_COMPILE_SRC_LABEL.to_string()],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

    fn addr() -> Addr {
        Addr::new(
            PkgBuf::from("mylib"),
            "go_compile_src".to_string(),
            Default::default(),
        )
    }

    fn deps(spec: &TargetSpec) -> Vec<String> {
        match spec.config.get("deps").unwrap() {
            Value::List(v) => v
                .iter()
                .map(|e| match e {
                    Value::String(s) => s.clone(),
                    _ => panic!("expected string dep"),
                })
                .collect(),
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn uses_group_driver_and_label() {
        let spec = build_spec(addr(), &["//mylib:foo.go".to_string()]);
        assert_eq!(spec.driver, hbuiltins::plugingroup::DRIVER_NAME);
        assert!(spec.labels.contains(&GO_COMPILE_SRC_LABEL.to_string()));
    }

    #[test]
    fn deps_are_the_source_addrs() {
        let src = vec!["//mylib:a.go".to_string(), "//mylib:b.go".to_string()];
        let spec = build_spec(addr(), &src);
        assert_eq!(deps(&spec), src);
    }
}
