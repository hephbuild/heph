use crate::engine::Engine;
use crate::engine::provider::ListPackagesRequest;
use crate::engine::request_state::RequestState;
use crate::hmemoizer::unwrap_arc_err;
use crate::htmatcher;
use crate::htpkg::PkgBuf;
use enclose::enclose;
use std::sync::Arc;

impl Engine {
    pub async fn packages(
        &self,
        m: &htmatcher::Matcher,
        rs: &Arc<RequestState>,
    ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<String>>>> {
        let prefix = match m {
            htmatcher::Matcher::Package(p) => p.clone(),
            htmatcher::Matcher::PackagePrefix(p) => p.clone(),
            _ => PkgBuf::from(""),
        };

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
