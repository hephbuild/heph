use std::collections::HashSet;
use crate::engine::Engine;
use crate::engine::driver::ParseRequest;
use crate::engine::provider::{GetError, GetRequest, GetResponse, ListRequest};
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htmatcher::{self, MatchResult};
use crate::htpkg::PkgBuf;

impl Engine {
    pub async fn query(&self, m: &htmatcher::Matcher, ctoken: &dyn Cancellable) -> anyhow::Result<Vec<Addr>> {
        let request_id = "".to_string();

        let pkg_list: Vec<String> = {
            let it = self.packages(m, ctoken).await?;
            let mut seen = HashSet::new();
            it.collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .filter(|p| seen.insert(p.clone()))
                .collect()
        };

        let mut results = Vec::new();

        for pkg_str in pkg_list {
            let pkg = PkgBuf::from(pkg_str);

            for provider in &self.providers {
                let addr_list: Vec<Addr> = {
                    let it = provider.provider.list(ListRequest {
                        request_id: request_id.clone(),
                        package: pkg.clone(),
                    }, ctoken).await?;
                    it.collect::<anyhow::Result<Vec<_>>>()?
                        .into_iter()
                        .map(|r| r.addr)
                        .filter(|a| a.package == pkg)
                        .collect()
                };

                for addr in addr_list {
                    match m.matches_addr(&addr) {
                        MatchResult::MatchYes => {
                            results.push(addr);
                        }
                        MatchResult::MatchNo => {}
                        MatchResult::MatchShrug => {
                            let spec = match provider.provider.get(GetRequest {
                                request_id: request_id.clone(),
                                addr: addr.clone(),
                                states: vec![],
                            }, ctoken).await {
                                Ok(GetResponse { target_spec }) => target_spec,
                                Err(GetError::NotFound) => continue,
                                Err(GetError::Other(e)) => return Err(e),
                            };

                            match m.matches_spec(&spec) {
                                MatchResult::MatchYes => {
                                    results.push(addr);
                                }
                                MatchResult::MatchNo => {}
                                MatchResult::MatchShrug => {
                                    let driver = match self.drivers_by_name.get(&spec.driver) {
                                        Some(d) => d,
                                        None => continue,
                                    };

                                    let def = driver.driver.parse(ParseRequest {
                                        request_id: request_id.clone(),
                                        target_spec: spec,
                                    }, ctoken).await?.target_def;

                                    if m.matches(&def) == MatchResult::MatchYes {
                                        results.push(addr);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use crate::engine::Config;
    use crate::engine::provider::{StaticProvider, TargetSpec};
    use crate::hasync::StdCancellationToken;
    use crate::htmatcher::Matcher;
    use tempfile::tempdir;

    fn make_spec(pkg: &str, name: &str, labels: &[&str]) -> TargetSpec {
        TargetSpec {
            addr: Addr {
                package: PkgBuf::from(pkg),
                name: name.to_string(),
                args: HashMap::new(),
            },
            driver: "exec".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn query_by_package() -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config { root: root.path().to_path_buf() })?;

        engine.register_provider(|_| Box::new(StaticProvider {
            targets: vec![
                make_spec("foo/bar", "a", &[]),
                make_spec("foo/bar", "b", &[]),
                make_spec("other", "c", &[]),
            ],
            packages: OnceLock::new(),
        }))?;

        let ctoken = StdCancellationToken::new();
        let addrs = engine.query(&Matcher::Package(PkgBuf::from("foo/bar")), &ctoken).await?;

        assert_eq!(addrs.len(), 2);
        assert!(addrs.iter().any(|a| a.name == "a"));
        assert!(addrs.iter().any(|a| a.name == "b"));
        Ok(())
    }

    #[tokio::test]
    async fn query_by_addr() -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config { root: root.path().to_path_buf() })?;

        engine.register_provider(|_| Box::new(StaticProvider {
            targets: vec![
                make_spec("foo", "a", &[]),
                make_spec("foo", "b", &[]),
            ],
            packages: OnceLock::new(),
        }))?;

        let ctoken = StdCancellationToken::new();
        let target_addr = Addr {
            package: PkgBuf::from("foo"),
            name: "a".to_string(),
            args: HashMap::new(),
        };
        let addrs = engine.query(&Matcher::Addr(target_addr), &ctoken).await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_by_label_calls_get_spec() -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config { root: root.path().to_path_buf() })?;

        engine.register_provider(|_| Box::new(StaticProvider {
            targets: vec![
                make_spec("foo", "a", &["//labels:lint"]),
                make_spec("foo", "b", &[]),
            ],
            packages: OnceLock::new(),
        }))?;

        let ctoken = StdCancellationToken::new();
        let label_addr = Addr {
            package: PkgBuf::from("labels"),
            name: "lint".to_string(),
            args: HashMap::new(),
        };
        let addrs = engine.query(&Matcher::Label(label_addr), &ctoken).await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_empty_when_no_match() -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config { root: root.path().to_path_buf() })?;

        engine.register_provider(|_| Box::new(StaticProvider {
            targets: vec![make_spec("foo", "a", &[])],
            packages: OnceLock::new(),
        }))?;

        let ctoken = StdCancellationToken::new();
        let addrs = engine.query(&Matcher::Package(PkgBuf::from("nonexistent")), &ctoken).await?;

        assert!(addrs.is_empty());
        Ok(())
    }
}
