pub const VERSION: &str = match option_env!("HEPH_BUILD_VERSION") {
    Some(val) => val,
    None => "v0.0.0-dev",
};

/// A [semver](https://semver.org/) version decomposed into its segments.
///
/// `major`/`minor`/`patch` are the numeric core. `pre_release` is the dot-joined
/// pre-release identifiers (the `-` segment), `build` is the build metadata (the
/// `+` segment) — both `None` when absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer<'a> {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre_release: Option<&'a str>,
    pub build: Option<&'a str>,
}

/// Parse a semver string into its segments, tolerating an optional leading `v`
/// (heph stamps versions as e.g. `v0.0.0-dev`). Returns `None` if the numeric
/// core is malformed, so callers can simply omit the segments rather than report
/// junk.
pub fn parse(version: &str) -> Option<SemVer<'_>> {
    let version = version.strip_prefix('v').unwrap_or(version);

    // Split off build metadata (`+`) first — it can itself contain `-`, so it
    // must be peeled before the pre-release `-`.
    let (rest, build) = match version.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (version, None),
    };
    let (core, pre_release) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };

    let mut nums = core.split('.');
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next()?.parse().ok()?;
    let patch = nums.next()?.parse().ok()?;
    if nums.next().is_some() {
        return None;
    }

    Some(SemVer {
        major,
        minor,
        patch,
        pre_release: pre_release.filter(|s| !s.is_empty()),
        build: build.filter(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_version() {
        assert_eq!(
            parse("v1.2.3-alpha.1+build.7"),
            Some(SemVer {
                major: 1,
                minor: 2,
                patch: 3,
                pre_release: Some("alpha.1"),
                build: Some("build.7"),
            })
        );
    }

    #[test]
    fn parses_without_v_prefix() {
        assert_eq!(
            parse("10.20.30"),
            Some(SemVer {
                major: 10,
                minor: 20,
                patch: 30,
                pre_release: None,
                build: None,
            })
        );
    }

    #[test]
    fn parses_dev_default() {
        assert_eq!(
            parse(VERSION),
            Some(SemVer {
                major: 0,
                minor: 0,
                patch: 0,
                pre_release: Some("dev"),
                build: None,
            })
        );
    }

    #[test]
    fn build_metadata_may_contain_hyphen() {
        let v = parse("v1.0.0+exp-sha.5114f85").unwrap();
        assert_eq!(v.pre_release, None);
        assert_eq!(v.build, Some("exp-sha.5114f85"));
    }

    #[test]
    fn pre_release_only() {
        let v = parse("2.3.4-rc.1").unwrap();
        assert_eq!(v.pre_release, Some("rc.1"));
        assert_eq!(v.build, None);
    }

    #[test]
    fn rejects_malformed_core() {
        assert_eq!(parse("1.2"), None);
        assert_eq!(parse("1.2.3.4"), None);
        assert_eq!(parse("v1.x.3"), None);
        assert_eq!(parse(""), None);
    }
}
