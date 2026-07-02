use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;
use std::collections::HashSet;

/// Label carried by the `go_compile_src` group so it can be selected by query
/// (e.g. `label(go_compile_src)`).
pub const GO_COMPILE_SRC_LABEL: &str = "go_compile_src";

/// Build the `go_compile_src` group: a transparent aggregate of every source
/// input the package's library compile consumes — on-disk and generated Go
/// sources plus embed assets, passed as separate lanes (`go_files`,
/// `embed_files`, `embed_src`). Deps are already-formatted target addresses.
/// Duplicates are dropped while preserving first-seen order so an addr that
/// appears in two lanes is listed once.
pub fn build_spec(addr: Addr, src_addr_lanes: &[&[String]]) -> TargetSpec {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut deps: Vec<Value> = Vec::new();
    for lane in src_addr_lanes {
        for a in *lane {
            if seen.insert(a.as_str()) {
                deps.push(Value::String(a.clone()));
            }
        }
    }

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
        let spec = build_spec(addr(), &[&["//mylib:foo.go".to_string()]]);
        assert_eq!(spec.driver, hbuiltins::plugingroup::DRIVER_NAME);
        assert!(spec.labels.contains(&GO_COMPILE_SRC_LABEL.to_string()));
    }

    #[test]
    fn concatenates_lanes_in_order() {
        let go = vec!["//mylib:a.go".to_string(), "//mylib:b.go".to_string()];
        let embed = vec!["//mylib:data.txt".to_string()];
        let spec = build_spec(addr(), &[&go, &embed]);
        assert_eq!(
            deps(&spec),
            vec![
                "//mylib:a.go".to_string(),
                "//mylib:b.go".to_string(),
                "//mylib:data.txt".to_string(),
            ]
        );
    }

    #[test]
    fn dedups_across_lanes_preserving_first_seen() {
        let go = vec!["//mylib:a.go".to_string()];
        let embed = vec!["//mylib:a.go".to_string(), "//mylib:data.txt".to_string()];
        let spec = build_spec(addr(), &[&go, &embed]);
        assert_eq!(
            deps(&spec),
            vec!["//mylib:a.go".to_string(), "//mylib:data.txt".to_string()]
        );
    }
}
