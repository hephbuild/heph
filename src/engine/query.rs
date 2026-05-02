use std::sync::Arc;
use futures::Stream;
use crate::engine::Engine;
use crate::engine::provider::ListRequest;
use crate::engine::request_state::RequestState;
use crate::htaddr::Addr;
use crate::htmatcher;
use crate::htmatcher::{MatchResult};
use crate::htpkg::PkgBuf;

impl Engine {
    pub fn query<'a>(&'a self, rs: Arc<RequestState>, m: &'a htmatcher::Matcher) -> impl Stream<Item = anyhow::Result<Addr>> + 'a {
        async_stream::try_stream! {
            for pkg_result in self.packages(m, &rs.ctoken).await? {
                let pkg = PkgBuf::from(pkg_result?);

                for provider in &self.providers {
                    let it = provider.provider.list(ListRequest {
                        request_id: rs.request_id.clone(),
                        package: pkg.clone(),
                    }, &rs.ctoken).await?;

                    for item in it {
                        let addr = item?.addr;

                        if addr.package != pkg {
                            continue;
                        }

                        match m.matches_addr(&addr) {
                            MatchResult::MatchYes => yield addr,
                            MatchResult::MatchNo => {}
                            MatchResult::MatchShrug => {
                                let spec = self.get_spec(rs.clone(), &addr).await?;

                                match m.matches_spec(&spec) {
                                    MatchResult::MatchYes => yield addr,
                                    MatchResult::MatchNo => {}
                                    MatchResult::MatchShrug => {
                                        let def = self.get_def(rs.clone(), &addr).await?;

                                        if m.matches(&def) == MatchResult::MatchYes {
                                            yield addr;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use crate::engine::Config;
    use crate::htmatcher::Matcher;
    use crate::pluginstatictarget;
    use tempfile::tempdir;

    fn target(pkg: &str, name: &str, labels: &[&str]) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: format!("//{pkg}:{name}"),
            driver: "exec".to_string(),
            run: None,
            out: None,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn make_engine(targets: Vec<pluginstatictarget::Target>) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config { root: root.path().to_path_buf() })?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    #[tokio::test]
    async fn query_by_package() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo/bar", "a", &[]),
            target("foo/bar", "b", &[]),
            target("other", "c", &[]),
        ])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine.query(rs, &Matcher::Package(PkgBuf::from("foo/bar"))).try_collect().await?;

        assert_eq!(addrs.len(), 2);
        assert!(addrs.iter().any(|a| a.name == "a"));
        assert!(addrs.iter().any(|a| a.name == "b"));
        Ok(())
    }

    #[tokio::test]
    async fn query_by_addr() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo", "a", &[]),
            target("foo", "b", &[]),
        ])?;

        let rs = engine.new_state();
        let target_addr = Addr {
            package: PkgBuf::from("foo"),
            name: "a".to_string(),
            args: Default::default(),
        };
        let addrs: Vec<Addr> = engine.query(rs, &Matcher::Addr(target_addr)).try_collect().await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_by_label_calls_get_spec() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo", "a", &["//labels:lint"]),
            target("foo", "b", &[]),
        ])?;

        let rs = engine.new_state();
        let label_addr = Addr {
            package: PkgBuf::from("labels"),
            name: "lint".to_string(),
            args: Default::default(),
        };
        let addrs: Vec<Addr> = engine.query(rs, &Matcher::Label(label_addr)).try_collect().await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_empty_when_no_match() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &[])])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine.query(rs, &Matcher::Package(PkgBuf::from("nonexistent"))).try_collect().await?;

        assert!(addrs.is_empty());
        Ok(())
    }
}
