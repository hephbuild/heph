use heph_core::debug_hash::DebugHasher;
use crate::engine::request_state::RequestState;
use crate::engine::result::ExtendedTargetDef;
use crate::engine::{EResult, Engine, OutputMatcher, ResultOptions};
use heph_core::hmemoizer::unwrap_arc_err;
use heph_model::htaddr::Addr;
use anyhow::Context;
use async_recursion::async_recursion;
use enclose::enclose;
use std::hash::Hasher;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

#[derive(Clone)]
pub struct ResultMeta {
    pub hashin: String,
}

impl Engine {
    pub async fn meta(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<ResultMeta> {
        let res = rs
            .data
            .mem_meta
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.inner_meta(rs, &addr).await
                }),
            )
            .await
            .map_err(unwrap_arc_err)?;
        Ok(res)
    }

    #[async_recursion]
    async fn inner_meta(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<ResultMeta> {
        // _no_track: meta is invoked from inner_result_addr after parent was
        // already set to addr — tracked get_def would record addr→addr.
        let def = Arc::clone(&self).get_def_no_track(rs.clone(), addr).await?;
        let results = self.clone().inputs_result_meta(rs.clone(), addr).await?;

        // Borrow the input hashouts rather than cloning each `String`: they live
        // in `results` (held until `hashin` returns) and the hash only reads their
        // bytes. Sorting `&str` by content yields the same order as sorting the
        // owned strings, so the digest is byte-identical.
        let mut hashouts: Vec<&str> = results
            .iter()
            .flat_map(|res| res.artifacts_meta.iter().map(|m| m.hashout.as_str()))
            .collect();
        hashouts.sort_unstable();

        let hashin = self
            .hashin(&def, hashouts.into_iter())
            .with_context(|| "hashin")?;

        Ok(ResultMeta { hashin })
    }

    pub(crate) fn hashin<'a>(
        &self,
        def: &ExtendedTargetDef,
        results: impl Iterator<Item = &'a str>,
    ) -> anyhow::Result<String> {
        let mut h = DebugHasher::new(Xxh3Default::new(), || {
            format!("hashin_{}", def.target_def.addr.format())
        });

        // Driver name is part of the cache key: swapping drivers under the
        // same addr (e.g. `bash` → `sh`, or a custom driver re-registration)
        // must invalidate cached artifacts even when the produced
        // `TargetDef` bytes happen to match.
        Hasher::write(&mut h, def.driver.as_bytes());
        Hasher::write(&mut h, &[0]);

        Hasher::write(&mut h, &def.target_def.hash);

        for hashout in results {
            Hasher::write(&mut h, hashout.as_bytes());
        }

        Ok(format!("{:x}", h.finish()))
    }

    async fn inputs_result_meta(
        self: Arc<Self>,
        rc: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Vec<Arc<EResult>>> {
        let inputs = Arc::clone(&self)
            .expanded_inputs_for(rc.clone(), addr)
            .await?;

        let fail_fast = rc.fail_fast();
        // Only `hashed=true` inputs contribute their hashout to the parent's
        // hashin. `runtime_deps` (hashed=false) are still materialized via
        // the link/execute path, but must not alter the cache key.
        let futures = inputs.iter().filter(|input| input.hashed).map(|input| {
            enclose!((self => engine, rc, input) async move {
                engine
                    .clone()
                    .result_addr(
                        rc,
                        &input.r#ref.r#ref,
                        OutputMatcher::None,
                        &ResultOptions::default(),
                    )
                    .await
            })
        });

        crate::engine::fanout::join_all_failable(futures, fail_fast).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::driver::targetdef::{CacheConfig, TargetDef};
    use heph_model::htpkg::PkgBuf;
    use std::collections::BTreeMap;

    fn test_engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        (engine, dir)
    }

    fn ext_def(driver: &str, hash: Vec<u8>) -> ExtendedTargetDef {
        let target_def = Arc::new(TargetDef {
            addr: Addr::new(PkgBuf::from("pkg"), "tgt".to_string(), BTreeMap::new()),
            labels: Vec::new(),
            raw_def: Arc::new(()),
            inputs: Vec::new(),
            outputs: Vec::new(),
            support_files: Vec::new(),
            cache: CacheConfig::on(true),
            pty: false,
            hash,
            transparent: false,
        });
        ExtendedTargetDef {
            target_def,
            applied_transitive: None,
            driver: driver.to_string(),
        }
    }

    /// Golden freeze of the cache-key digest. The hashin folds driver name, the
    /// target-def hash, and the (already-sorted) input hashouts. T1.2 switched the
    /// caller from cloning + `itertools::sorted()` owned `String`s to borrowing +
    /// `sort_unstable()` on `&str`; both must hash byte-identically. A change to
    /// this constant means the cache key drifted — every cached artifact across
    /// versions would silently invalidate, so this value must not change casually.
    #[test]
    fn hashin_is_stable_and_keyed_on_driver_and_inputs() {
        let (engine, _dir) = test_engine();
        let def = ext_def("bash", vec![1, 2, 3]);

        let h = engine
            .hashin(&def, ["aaa", "bbb", "ccc"].into_iter())
            .expect("hashin");
        assert_eq!(
            h, "49fb4d47cf278c4f",
            "cache-key digest must stay stable across the borrow/sort refactor"
        );

        // Driver name and input hashouts both fold into the key.
        assert_ne!(
            h,
            engine
                .hashin(
                    &ext_def("sh", vec![1, 2, 3]),
                    ["aaa", "bbb", "ccc"].into_iter()
                )
                .unwrap(),
            "driver name is part of the cache key"
        );
        assert_ne!(
            h,
            engine
                .hashin(&def, ["aaa", "bbb", "ddd"].into_iter())
                .unwrap(),
            "input hashouts are part of the cache key"
        );
    }
}
