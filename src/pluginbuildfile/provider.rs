use std::collections::HashMap;
use std::sync::Mutex;
use futures::future::BoxFuture;
use crate::engine::provider::{ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider as EProvider, State, TargetSpec};
use crate::engine::provider::GetError::NotFound;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;

pub struct RequestState {

}

pub struct Provider {
    pub root: std::path::PathBuf,
    pub requests: Mutex<HashMap<String, RequestState>>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            root: std::path::PathBuf::from("/"),
            requests: Mutex::new(HashMap::new()),
        }
    }
}

impl Provider {
    fn find_packages(&self, path: &std::path::Path, packages: &mut std::collections::HashSet<String>) -> anyhow::Result<()> {
        let mut has_build_file = false;
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let entry_path = entry.path();
            if entry_path.is_file() && entry_path.file_name().and_then(|n| n.to_str()) == Some("BUILD") {
                has_build_file = true;
            } else if entry_path.is_dir() {
                self.find_packages(&entry_path, packages)?;
            }
        }

        if has_build_file {
            let mut current = path;
            while let Ok(rel) = current.strip_prefix(&self.root) {
                let pkg_name = rel.to_string_lossy().to_string();
                packages.insert(pkg_name);

                if let Some(parent) = current.parent() {
                    current = parent;
                } else {
                    break;
                }
            }
        }

        Ok(())
    }
}

impl EProvider for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse{
            name: "buildfile".to_string(),
        })
    }

    fn list<'a>(&'a self, req: ListRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>> {
        Box::pin(async move {
            let res = self.run_pkg(&req.package)?;

            let items: Vec<anyhow::Result<ListResponse>> = res.targets.into_iter().map(|p| {
                Ok(ListResponse {
                    addr: Addr {
                        package: req.package.clone(),
                        name: p.name,
                        args: Default::default(),
                    },
                })
            }).collect();

            Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
        })
    }

    fn list_packages<'a>(&'a self, _req: ListPackagesRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>> {
        Box::pin(async move {
            let mut packages = std::collections::HashSet::new();

            self.find_packages(&self.root, &mut packages)?;

            let items: Vec<anyhow::Result<ListPackageResponse>> = packages.into_iter().map(|p| {
                Ok(ListPackageResponse {
                    pkg: p,
                })
            }).collect();

            Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>)
        })
    }

    fn get<'a>(&'a self, req: GetRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            let res = self.run_pkg(&req.addr.package).map_err(|e: anyhow::Error| GetError::Other(e))?;

            for p in res.targets {
                if p.name == req.addr.name {
                    return Ok(GetResponse{
                        target_spec: TargetSpec{
                            addr: req.addr.clone(),
                            driver: p.driver,
                            config: p.config,
                            labels: p.labels,
                            transitive: Default::default(),
                        },
                    })
                }
            }

            Err(NotFound)
        })
    }

    fn probe<'a>(&'a self, req: ProbeRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            let res = self.run_pkg(&req.package)?;

            Ok(ProbeResponse{
                states: res.states.into_iter().map(|_p| {
                    State{
                        package: req.package.clone(),
                        provider: "buildfile".to_string(), // TODO: move into engine
                        state: Default::default(),
                    }
                }).collect(),
            })
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_list_packages() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   BUILD (root package "")
        //   a/
        //     b/
        //       BUILD
        //   c/
        //     BUILD

        fs::write(root.join("BUILD"), "").unwrap();
        let ab = root.join("a").join("b");
        fs::create_dir_all(&ab).unwrap();
        fs::write(ab.join("BUILD"), "").unwrap();

        let c = root.join("c");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("BUILD"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            requests: Mutex::new(HashMap::new()),
        };

        let req = ListPackagesRequest { prefix: "".to_string() };
        struct NoopCancellationToken;
        impl crate::hasync::Cancellable for NoopCancellationToken {
            fn is_cancelled(&self) -> bool { false }
            fn cancelled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
                Box::pin(std::future::pending())
            }
        }
        let ctoken = NoopCancellationToken;
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg).collect();

        assert_eq!(packages.len(), 4);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a/b".to_string()));
        assert!(packages.contains(&"a".to_string())); // parent of a/b
        assert!(packages.contains(&"c".to_string()));
    }
}
