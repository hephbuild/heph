use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;

fn resolve_relative_pkg(base: &PkgBuf, rel: &str) -> anyhow::Result<String> {
    let mut components: Vec<&str> = base.as_str().split('/').filter(|s| !s.is_empty()).collect();
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    for component in rel.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(anyhow::anyhow!("relative path '{}' escapes root", rel));
                }
            }
            c => components.push(c),
        }
    }
    Ok(components.join("/"))
}

pub fn parse(input: &str, base: &PkgBuf) -> anyhow::Result<Matcher> {
    // Absolute package reference
    if let Some(abs) = input.strip_prefix("//") {
        // Check for `...` suffix
        if let Some(prefix) = abs.strip_suffix("/...") {
            return Ok(Matcher::PackagePrefix(PkgBuf::from(prefix)));
        }
        return Ok(Matcher::Package(PkgBuf::from(abs)));
    }

    // Relative package reference
    if input.starts_with("./") || input.starts_with("../") {
        // Extract the path portion before `...` if present
        let (path_part, has_ellipsis) = if let Some(p) = input.strip_suffix("/...") {
            (p, true)
        } else {
            (input, false)
        };

        let pkg = resolve_relative_pkg(base, path_part)?;

        if has_ellipsis {
            return Ok(Matcher::PackagePrefix(PkgBuf::from(pkg)));
        } else {
            return Ok(Matcher::Package(PkgBuf::from(pkg)));
        }
    }

    Err(anyhow::anyhow!("invalid package reference: '{}' (must start with '//', './', or '../')", input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_absolute_package() {
        let base = PkgBuf::from("base/pkg");
        let m = parse("//foo/bar", &base).unwrap();
        assert_eq!(m, Matcher::Package(PkgBuf::from("foo/bar")));
    }

    #[test]
    fn test_absolute_package_prefix() {
        let base = PkgBuf::from("base/pkg");
        let m = parse("//foo/bar/...", &base).unwrap();
        assert_eq!(m, Matcher::PackagePrefix(PkgBuf::from("foo/bar")));
    }

    #[test]
    fn test_relative_dot_slash() {
        let base = PkgBuf::from("a/b");
        let m = parse("./sub", &base).unwrap();
        assert_eq!(m, Matcher::Package(PkgBuf::from("a/b/sub")));
    }

    #[test]
    fn test_relative_dot_slash_prefix() {
        let base = PkgBuf::from("a/b");
        let m = parse("./sub/...", &base).unwrap();
        assert_eq!(m, Matcher::PackagePrefix(PkgBuf::from("a/b/sub")));
    }

    #[test]
    fn test_relative_dot_dot_slash() {
        let base = PkgBuf::from("a/b/c");
        let m = parse("../sibling", &base).unwrap();
        assert_eq!(m, Matcher::Package(PkgBuf::from("a/b/sibling")));
    }

    #[test]
    fn test_relative_dot_dot_slash_prefix() {
        let base = PkgBuf::from("a/b/c");
        let m = parse("../sibling/...", &base).unwrap();
        assert_eq!(m, Matcher::PackagePrefix(PkgBuf::from("a/b/sibling")));
    }

    #[test]
    fn test_relative_multiple_dot_dot() {
        let base = PkgBuf::from("a/b/c/d");
        let m = parse("../../other", &base).unwrap();
        assert_eq!(m, Matcher::Package(PkgBuf::from("a/b/other")));
    }

    #[test]
    fn test_relative_escapes_root_fails() {
        let base = PkgBuf::from("a");
        let res = parse("../../escape", &base);
        assert!(res.is_err());
    }

    #[test]
    fn test_absolute_root_package() {
        let base = PkgBuf::from("anywhere");
        let m = parse("//", &base).unwrap();
        assert_eq!(m, Matcher::Package(PkgBuf::from("")));
    }

    #[test]
    fn test_absolute_root_package_prefix() {
        let base = PkgBuf::from("anywhere");
        let m = parse("//...", &base).unwrap();
        assert_eq!(m, Matcher::PackagePrefix(PkgBuf::from("")));
    }

    #[test]
    fn test_invalid_no_prefix() {
        let base = PkgBuf::from("a/b");
        let res = parse("foo/bar", &base);
        assert!(res.is_err());
    }

    #[test]
    fn test_relative_current_package() {
        let base = PkgBuf::from("a/b");
        let m = parse("./", &base).unwrap();
        assert_eq!(m, Matcher::Package(PkgBuf::from("a/b")));
    }

    #[test]
    fn test_relative_current_package_prefix() {
        let base = PkgBuf::from("a/b");
        let m = parse("./...", &base).unwrap();
        assert_eq!(m, Matcher::PackagePrefix(PkgBuf::from("a/b")));
    }
}
