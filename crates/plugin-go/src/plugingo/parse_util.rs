//! What the Go drivers' `parse` share besides their deps (for those, see
//! [`deps`](super::deps)): package-relative outputs and the def hash.

use hcore::debug_hash::DebugHasher;
use hmodel::htaddr::Addr;
use hplugin::driver::targetdef::Output;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use xxhash_rust::xxh3::Xxh3Default;

/// `path` (package-relative) as a workspace-relative path.
pub(crate) fn pkg_path(pkg: &str, path: &str) -> String {
    if pkg.is_empty() {
        path.to_string()
    } else {
        format!("{pkg}/{path}")
    }
}

/// A target's `out`, each package-relative path a collected file output under
/// `pkg`, written back as `codegen` says.
pub(crate) fn pkg_outputs(
    pkg: &str,
    out: &HashMap<String, Vec<String>>,
    codegen: &CodegenMode,
) -> Vec<Output> {
    out.iter()
        .map(|(group, paths)| Output {
            group: group.clone(),
            paths: paths
                .iter()
                .map(|p| Path {
                    content: Content::FilePath(pkg_path(pkg, p)),
                    codegen_tree: codegen.clone(),
                    collect: true,
                })
                .collect(),
        })
        .collect()
}

/// The def hash of a `driver` target at `addr`: `def`'s `Hash`, as lowercase hex
/// bytes. `driver` only names the `HEPH_DEBUG_HASH` dump.
pub(crate) fn def_hash(driver: &str, addr: &Addr, def: &impl Hash) -> Vec<u8> {
    let mut h = DebugHasher::new(Xxh3Default::new(), || format!("{driver}_{}", addr.format()));
    def.hash(&mut h);
    format!("{:x}", h.finish()).into_bytes()
}
