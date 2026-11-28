use crate::engine::expand::{ExpandedInputs, PartialExpansion};
use crate::engine::request_state::{AddrKey, RequestState};
use crate::engine::result::ExtendedTargetDef;
use crate::engine::{EResult, Engine, OutputMatcher, ResultOptions};
use anyhow::Context;
use async_recursion::async_recursion;
use enclose::enclose;
use hcore::debug_hash::DebugHasher;
use hcore::hmemoizer::{downcast_chain_ref, unwrap_arc_err};
use hmodel::htaddr::Addr;
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
                AddrKey(addr.clone()),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.inner_meta(rs, &addr).await
                }),
            )
            .await
            .map_err(unwrap_arc_err)?;
        Ok(res)
    }

    /// How many distinct targets [`meta`](Self::meta) needs the results of
    /// before it can hash `addr`: what it builds, or finds cached. The
    /// expansion is memoized in `rs`, so a `meta` on the same state that
    /// follows does not expand the inputs again.
    pub async fn hash_dep_count(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<usize> {
        let inputs = self.expanded_inputs_for(rs, addr).await?;
        Ok(inputs
            .iter()
            .filter(|input| input.hashed)
            .map(|input| &input.r#ref.r#ref)
            .collect::<std::collections::HashSet<_>>()
            .len())
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
        // `inputs_result_meta` expands under `def.target_def.addr`; it must be
        // the addr this meta is for.
        debug_assert_eq!(def.target_def.addr, *addr);
        let (inputs, results) = self.clone().inputs_result_meta(rs.clone(), &def).await?;

        // `results` is the hashed inputs' results, index for index: each is
        // paired with its addr, which only the `HEPH_DEBUG_HASH` trace reads.
        let deps = inputs
            .iter()
            .filter(|input| input.hashed)
            .map(|input| &input.r#ref.r#ref)
            .zip(results.iter().map(|r| &**r));
        let hashin = self.hashin(&def, deps).with_context(|| "hashin")?;

        Ok(ResultMeta { hashin })
    }

    /// The cache key: the driver name, a `0` byte, the def hash, then the
    /// hashout of every artifact of the hashed deps `deps`, sorted and written
    /// raw. The hashouts are sorted here, so neither the deps' order nor an
    /// artifact's place in its dep's result reaches the key; they borrow from
    /// `deps`, so collecting them allocates one `Vec`.
    ///
    /// A dep's addr reaches no key: under `HEPH_DEBUG_HASH` each artifact is
    /// noted in the trace as `dep <addr>: <hashout>` (a note hashes nothing),
    /// so a diff of two traces says which dep moved.
    pub(crate) fn hashin<'a>(
        &self,
        def: &ExtendedTargetDef,
        deps: impl IntoIterator<Item = (&'a Addr, &'a EResult)>,
    ) -> anyhow::Result<String> {
        let mut h = DebugHasher::new(Xxh3Default::new(), || {
            format!("hashin_{}", def.target_def.addr.format())
        });
        let mut hashouts: Vec<&str> = Vec::new();
        for (dep, res) in deps {
            for m in &res.artifacts_meta {
                h.note(|| format!("dep {dep}: {}", m.hashout));
                hashouts.push(m.hashout.as_str());
            }
        }
        hashouts.sort_unstable();

        // Driver name is part of the cache key: swapping drivers under the
        // same addr (e.g. `bash` → `sh`, or a custom driver re-registration)
        // must invalidate cached artifacts even when the produced
        // `TargetDef` bytes happen to match.
        Hasher::write(&mut h, def.driver.as_bytes());
        Hasher::write(&mut h, &[0]);

        Hasher::write(&mut h, &def.target_def.hash);

        for hashout in hashouts {
            Hasher::write(&mut h, hashout.as_bytes());
        }

        Ok(format!("{:x}", h.finish()))
    }

    /// The expanded inputs of `def`, and the result of each hashed one, in the
    /// order the hashed inputs appear in them.
    ///
    /// An expansion that failed is never hashed: this fails with its errors.
    /// It first builds the hashed inputs that resolved (D1.f), so their own
    /// failures are found and their successes cached, as they would have been
    /// had the broken input not been there. Runtime-only inputs that resolved
    /// stay unbuilt: nothing would consume them.
    async fn inputs_result_meta(
        self: Arc<Self>,
        rc: Arc<RequestState>,
        def: &ExtendedTargetDef,
    ) -> anyhow::Result<(Arc<ExpandedInputs>, Vec<Arc<EResult>>)> {
        let declared = &def.target_def;
        let inputs = match Arc::clone(&self)
            .expanded_inputs_for(rc.clone(), &declared.addr)
            .await
        {
            Ok(inputs) => inputs,
            Err(e) => {
                if let Some(partial) = downcast_chain_ref::<PartialExpansion>(&e) {
                    let hashed: Vec<&Addr> = partial
                        .resolved()
                        .filter(|(_, idx)| declared.inputs.get(*idx).is_some_and(|i| i.hashed))
                        .map(|(def, _)| &def.target_def.addr)
                        .collect();
                    // Each one's failure is its own scope's to record; the
                    // expansion's errors are what this target fails with.
                    let opts = ResultOptions::default();
                    let _built: Vec<anyhow::Result<Arc<EResult>>> =
                        futures::future::join_all(hashed.into_iter().map(|input| {
                            Arc::clone(&self).result_addr(
                                rc.clone(),
                                input,
                                OutputMatcher::None,
                                &opts,
                            )
                        }))
                        .await;
                }
                return Err(e);
            }
        };

        let fail_fast = rc.fail_fast();
        // Only `hashed=true` inputs contribute their hashout to the parent's
        // hashin. `runtime_deps` (hashed=false) are still materialized via
        // the link/execute path, but must not alter the cache key.
        let futures = inputs.iter().filter(|input| input.hashed).map(|input| {
            enclose!((self => engine, rc) async move {
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

        let results = crate::engine::fanout::join_all_failable(futures, fail_fast).await?;
        Ok((inputs, results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::driver::targetdef::{CacheConfig, TargetDef};
    use hmodel::htpkg::PkgBuf;
    use hplugin::eresult::ArtifactMeta;
    use std::collections::BTreeMap;

    fn test_engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(dir.path())
        })
        .expect("engine");
        (engine, dir)
    }

    fn ext_def(driver: &str, hash: Vec<u8>) -> ExtendedTargetDef {
        let target_def = Arc::new(TargetDef {
            addr: Addr::new(PkgBuf::from("pkg"), "tgt".to_string(), BTreeMap::new()),
            cache: CacheConfig::on(true),
            hash,
            ..Default::default()
        });
        ExtendedTargetDef {
            target_def,
            applied_transitive: None,
            driver: driver.to_string(),
        }
    }

    fn dep(hashouts: &[&str]) -> EResult {
        EResult {
            artifacts_meta: hashouts
                .iter()
                .map(|h| ArtifactMeta {
                    hashout: (*h).to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn dep_addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("dep"), name.to_string(), BTreeMap::new())
    }

    /// The hashin of `def` whose hashed deps resolved to `deps`, each under the
    /// addr `//dep:<index>`.
    fn hashin_of(engine: &Engine, def: &ExtendedTargetDef, deps: &[EResult]) -> String {
        let addrs: Vec<Addr> = (0..deps.len()).map(|i| dep_addr(&i.to_string())).collect();
        engine.hashin(def, addrs.iter().zip(deps)).expect("hashin")
    }

    /// The hashin of a `bash` target whose hashed deps resolved to `deps`.
    fn hashin_over(engine: &Engine, deps: &[EResult]) -> String {
        hashin_of(engine, &ext_def("bash", vec![1, 2, 3]), deps)
    }

    /// Golden freeze of the cache-key digest. The hashin folds driver name, the
    /// target-def hash, and the sorted input hashouts. T1.2 switched the
    /// caller from cloning + `itertools::sorted()` owned `String`s to borrowing +
    /// `sort_unstable()` on `&str`; both must hash byte-identically. A change to
    /// this constant means the cache key drifted — every cached artifact across
    /// versions would silently invalidate, so this value must not change casually.
    #[test]
    fn hashin_is_stable_and_keyed_on_driver_and_inputs() {
        let (engine, _dir) = test_engine();
        let def = ext_def("bash", vec![1, 2, 3]);
        let deps = [dep(&["ccc", "aaa"]), dep(&["bbb"])];

        let h = hashin_of(&engine, &def, &deps);
        assert_eq!(
            h, "49fb4d47cf278c4f",
            "cache-key digest must stay stable across the borrow/sort refactor"
        );

        // Driver name and input hashouts both fold into the key.
        assert_ne!(
            h,
            hashin_of(&engine, &ext_def("sh", vec![1, 2, 3]), &deps),
            "driver name is part of the cache key"
        );
        assert_ne!(
            h,
            hashin_of(&engine, &def, &[dep(&["aaa", "bbb", "ddd"])]),
            "input hashouts are part of the cache key"
        );
        // A dep's addr only names it in the debug trace.
        let renamed = [dep_addr("other"), dep_addr("names")];
        assert_eq!(
            h,
            engine.hashin(&def, renamed.iter().zip(&deps)).unwrap(),
            "a dep's addr is not part of the cache key"
        );
    }

    /// A target with no hashed dep, and one whose deps have no artifacts, fold
    /// nothing after the def hash: both key as the hashing has always keyed a
    /// dep-less target. The constant is the bare fold's (driver, `0`, def hash).
    #[test]
    fn hashin_of_deps_with_no_artifacts_is_that_of_no_deps() {
        let (engine, _dir) = test_engine();
        let none = hashin_over(&engine, &[]);
        assert_eq!(none, "55336fc88812b4d8", "a no-dep key must not move");
        assert_eq!(hashin_over(&engine, &[dep(&[])]), none);
        assert_eq!(hashin_over(&engine, &[dep(&[]), dep(&[])]), none);
    }

    /// A dep with many artifacts (a big glob, an unpacked tree) keys the same
    /// whatever order it lists them in, and however they split across deps.
    /// The constant is the bare fold of `h00000`..`h01499`, sorted.
    #[test]
    fn hashin_of_a_many_artifact_dep_ignores_their_order() {
        let (engine, _dir) = test_engine();
        let hashouts: Vec<String> = (0..1500).map(|i| format!("h{i:05}")).collect();
        let refs: Vec<&str> = hashouts.iter().map(String::as_str).collect();

        let forward = hashin_over(&engine, &[dep(&refs)]);
        assert_eq!(
            forward, "322f06133ba59372",
            "a many-artifact key must not move"
        );

        let mut reversed = refs.clone();
        reversed.reverse();
        assert_eq!(forward, hashin_over(&engine, &[dep(&reversed)]));
        // A deterministic shuffle: stride through the list by a step coprime
        // with its length.
        let shuffled: Vec<&str> = (0..refs.len())
            .map(|i| refs[(i * 7919) % refs.len()])
            .collect();
        assert_eq!(forward, hashin_over(&engine, &[dep(&shuffled)]));
        let (a, b) = refs.split_at(700);
        assert_eq!(forward, hashin_over(&engine, &[dep(b), dep(a)]));
    }
}
