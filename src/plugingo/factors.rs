use crate::htaddr::Addr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Factors {
    pub goos: String,
    pub goarch: String,
    pub build_tags: Vec<String>,
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
        Self {
            goos: addr.args.get("goos").cloned().unwrap_or_else(current_goos),
            goarch: addr
                .args
                .get("goarch")
                .cloned()
                .unwrap_or_else(current_goarch),
            build_tags: tags,
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
    rust_os_to_go(std::env::consts::OS).to_string()
}

fn rust_os_to_go(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other,
    }
}

pub fn current_goarch() -> String {
    rust_arch_to_go(std::env::consts::ARCH).to_string()
}

fn rust_arch_to_go(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htaddr::Addr;
    use crate::htpkg::PkgBuf;

    fn addr_with_args(args: &[(&str, &str)]) -> Addr {
        Addr {
            package: PkgBuf::from(""),
            name: String::new(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
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
        };
        assert!(f.go_list_flags().is_empty());
    }

    #[test]
    fn test_go_list_flags_with_tags() {
        let f = Factors {
            goos: "linux".into(),
            goarch: "amd64".into(),
            build_tags: vec!["foo".into(), "bar".into()],
        };
        assert_eq!(f.go_list_flags(), vec!["-tags", "foo,bar"]);
    }

    #[test]
    fn test_arch_mapping() {
        assert_eq!(rust_arch_to_go("x86_64"), "amd64");
        assert_eq!(rust_arch_to_go("aarch64"), "arm64");
        assert_eq!(rust_arch_to_go("x86"), "386");
        assert_eq!(rust_arch_to_go("arm"), "arm");
        assert_eq!(rust_arch_to_go("riscv64"), "riscv64");
    }

    #[test]
    fn test_os_mapping() {
        assert_eq!(rust_os_to_go("macos"), "darwin");
        assert_eq!(rust_os_to_go("linux"), "linux");
        assert_eq!(rust_os_to_go("windows"), "windows");
    }
}
