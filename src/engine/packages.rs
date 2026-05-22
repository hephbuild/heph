use crate::engine::Engine;
use crate::engine::provider::ListPackagesRequest;
use crate::engine::request_state::RequestState;
use crate::hmemoizer::unwrap_arc_err;
use crate::htmatcher;
use crate::htpkg::PkgBuf;
use enclose::enclose;
use std::sync::Arc;

/// Walk the matcher and pick the most specific `Package` / `PackagePrefix`
/// constraint that any path-to-MatchYes must satisfy. Used to narrow the
/// `list_packages` scan; the per-addr matcher still runs afterwards, so
/// over-broad prefixes only cost perf, never correctness.
///
/// `And` arms are intersected — pick the longest narrowing arm. `Or` / `Not`
/// can't be narrowed in general (any arm could match a different prefix),
/// so they fall back to empty.
fn narrowing_prefix(m: &htmatcher::Matcher) -> PkgBuf {
    match m {
        htmatcher::Matcher::Package(p) | htmatcher::Matcher::PackagePrefix(p) => p.clone(),
        htmatcher::Matcher::And(ms) => ms
            .iter()
            .map(narrowing_prefix)
            .max_by_key(|p| p.as_str().len())
            .unwrap_or_else(|| PkgBuf::from("")),
        _ => PkgBuf::from(""),
    }
}

impl Engine {
    pub async fn packages(
        &self,
        m: &htmatcher::Matcher,
        rs: &Arc<RequestState>,
    ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<String>>>> {
        let prefix = narrowing_prefix(m);

        let mut all_packages = Vec::new();

        for provider in &self.providers {
            let req = ListPackagesRequest {
                prefix: prefix.clone(),
            };
            let key = format!("{}:{}", provider.name, prefix);

            let pkgs = rs
                .data
                .mem_packages
                .once(
                    key,
                    enclose!((provider, rs) move || async move {
                        let it = provider
                            .provider
                            .list_packages(req, rs.ctoken())
                            .await?;
                        let mut pkgs = Vec::new();
                        for res in it {
                            pkgs.push(res?.pkg.to_string());
                        }
                        Ok(Arc::new(pkgs))
                    }),
                )
                .await
                .map_err(unwrap_arc_err)?;

            all_packages.extend((*pkgs).clone());
        }

        Ok(Box::new(all_packages.into_iter().map(Ok)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htmatcher::Matcher;

    fn pkg(s: &str) -> PkgBuf {
        PkgBuf::from(s)
    }

    #[test]
    fn package_returns_pkg() {
        assert_eq!(
            narrowing_prefix(&Matcher::Package(pkg("foo/bar"))),
            pkg("foo/bar")
        );
    }

    #[test]
    fn package_prefix_returns_pkg() {
        assert_eq!(
            narrowing_prefix(&Matcher::PackagePrefix(pkg("foo"))),
            pkg("foo")
        );
    }

    #[test]
    fn label_returns_empty() {
        assert_eq!(narrowing_prefix(&Matcher::Label("l".into())), pkg(""));
    }

    #[test]
    fn and_picks_package_arm() {
        let m = Matcher::And(vec![
            Matcher::Package(pkg("foo/bar")),
            Matcher::Label("go_test_data".into()),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("foo/bar"));
    }

    #[test]
    fn and_picks_longest_prefix_arm() {
        let m = Matcher::And(vec![
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::PackagePrefix(pkg("foo/bar")),
            Matcher::Label("l".into()),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("foo/bar"));
    }

    #[test]
    fn and_with_no_pkg_arms_returns_empty() {
        let m = Matcher::And(vec![Matcher::Label("a".into()), Matcher::Label("b".into())]);
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn or_returns_empty() {
        // Or arms could each match different prefixes; can't narrow safely.
        let m = Matcher::Or(vec![
            Matcher::Package(pkg("foo")),
            Matcher::Package(pkg("bar")),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn not_returns_empty() {
        let m = Matcher::Not(Box::new(Matcher::Package(pkg("foo"))));
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn nested_and_descends() {
        let m = Matcher::And(vec![
            Matcher::Label("l".into()),
            Matcher::And(vec![
                Matcher::Label("m".into()),
                Matcher::Package(pkg("deep/inner")),
            ]),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("deep/inner"));
    }
}
