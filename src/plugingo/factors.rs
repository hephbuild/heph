use crate::htaddr::Addr;
use std::collections::BTreeMap;

/// Recognized build-env customization knobs, as `(addr_arg_key, GO_ENV_VAR)` pairs.
/// Each maps a bare named address arg (e.g. `@goexperiment=rangefunc`) to the Go env
/// var set during compile/list/link. Add a row here to support a new knob — the rest
/// of the plumbing (parsing, hashing, env wiring) is generic over this table.
pub const ENV_FACTORS: &[(&str, &str)] =
    &[("goexperiment", "GOEXPERIMENT"), ("godebug", "GODEBUG")];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Factors {
    pub goos: String,
    pub goarch: String,
    pub build_tags: Vec<String>,
    /// Build-env customizations keyed by Go env var (e.g. `GOEXPERIMENT`). Sorted +
    /// hashable; flows into the hashed `env` of every build/test target and `go list`.
    pub env: BTreeMap<String, String>,
    /// `go tool link` flags — applied to the binary LINK STEP ONLY, never to compile
    /// or `go list`. Kept out of `factors_to_args` so the lib archive cache stays shared.
    pub ldflags: Vec<String>,
}

impl Factors {
    pub fn from_addr(addr: &Addr) -> Self {
        let mut tags: Vec<String> = addr
            .args
            .get("tags")
            .map(|t| {
                t.split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        tags.sort();
        let env = ENV_FACTORS
            .iter()
            .filter_map(|(key, go_var)| {
                addr.args.get(*key).map(|v| (go_var.to_string(), v.clone()))
            })
            .collect();
        let ldflags = addr
            .args
            .get("ldflags")
            .map(|v| v.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        Self {
            goos: addr.args.get("goos").cloned().unwrap_or_else(current_goos),
            goarch: addr
                .args
                .get("goarch")
                .cloned()
                .unwrap_or_else(current_goarch),
            build_tags: tags,
            env,
            ldflags,
        }
    }

    #[cfg(test)]
    pub fn go_list_flags(&self) -> Vec<String> {
        if self.build_tags.is_empty() {
            vec![]
        } else {
            vec!["-tags".to_string(), self.build_tags.join(",")]
        }
    }
}

pub fn current_goos() -> String {
    // Go's GOOS naming matches the canonical (Go/OCI) convention.
    crate::htplatform::os().to_string()
}

pub fn current_goarch() -> String {
    // Go's GOARCH naming matches the canonical (Go/OCI) convention.
    crate::htplatform::arch().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htaddr::Addr;
    use crate::htpkg::PkgBuf;

    fn addr_with_args(args: &[(&str, &str)]) -> Addr {
        Addr::new(
            PkgBuf::from(""),
            String::new(),
            args.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn test_from_addr_explicit() {
        let addr = addr_with_args(&[("goos", "linux"), ("goarch", "amd64"), ("tags", "foo,bar")]);
        let f = Factors::from_addr(&addr);
        assert_eq!(f.goos, "linux");
        assert_eq!(f.goarch, "amd64");
        assert_eq!(f.build_tags, vec!["bar", "foo"]); // sorted
    }

    #[test]
    fn test_from_addr_defaults() {
        let addr = Addr::default();
        let f = Factors::from_addr(&addr);
        assert!(!f.goos.is_empty());
        assert!(!f.goarch.is_empty());
        assert!(f.build_tags.is_empty());
    }

    #[test]
    fn test_go_list_flags_empty_tags() {
        let f = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec![],
            env: Default::default(),
            ldflags: vec![],
        };
        assert!(f.go_list_flags().is_empty());
    }

    #[test]
    fn test_go_list_flags_with_tags() {
        let f = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec!["foo".into(), "bar".into()],
            env: Default::default(),
            ldflags: vec![],
        };
        assert_eq!(f.go_list_flags(), vec!["-tags", "foo,bar"]);
    }

    #[test]
    fn test_from_addr_env_factors() {
        let addr = addr_with_args(&[
            ("goexperiment", "rangefunc,arenas"),
            ("godebug", "http2debug=1"),
        ]);
        let f = Factors::from_addr(&addr);
        assert_eq!(f.env.get("GOEXPERIMENT").unwrap(), "rangefunc,arenas");
        assert_eq!(f.env.get("GODEBUG").unwrap(), "http2debug=1");
    }

    #[test]
    fn test_from_addr_env_absent() {
        let addr = addr_with_args(&[("goos", "linux")]);
        let f = Factors::from_addr(&addr);
        assert!(f.env.is_empty());
        assert!(f.ldflags.is_empty());
    }

    #[test]
    fn test_from_addr_ldflags() {
        let addr = addr_with_args(&[("ldflags", "-s -w -X main.v=1.2")]);
        let f = Factors::from_addr(&addr);
        assert_eq!(f.ldflags, vec!["-s", "-w", "-X", "main.v=1.2"]);
    }
}
