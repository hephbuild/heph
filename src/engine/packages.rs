use crate::engine::Engine;
use crate::htmatcher;
use crate::hasync::Cancellable;
use crate::engine::provider::ListPackagesRequest;

impl Engine {
    pub async fn packages(&self, m: &htmatcher::Matcher, ctoken: &dyn Cancellable) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<String>>>> {
        let prefix = match m {
            htmatcher::Matcher::Package(p) => p.clone(),
            htmatcher::Matcher::PackagePrefix(p) => p.clone(),
            _ => "".to_string(), // Default or handle other cases
        };

        let mut all_packages = Vec::new();

        for provider in &self.providers {
            let req = ListPackagesRequest {
                prefix: prefix.clone(),
            };

            let it = provider.provider.list_packages(req, ctoken).await?;
            for res in it {
                let pkg_res = res?;
                all_packages.push(pkg_res.pkg);
            }
        }

        Ok(Box::new(all_packages.into_iter().map(Ok)))
    }
}