use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::plugingo::factors::Factors;
use crate::plugingo::pkg_analysis::GoPackage;
use crate::plugingo::target_lib;

pub fn build_spec(
    addr: Addr,
    pkg: &GoPackage,
    factors: &Factors,
    transitive_libs: &[(String, Addr)],
    go_bin_addr: &str,
    goroot: &str,
) -> TargetSpec {
    // Thirdparty source files live in GOMODCACHE, not the workspace — pluginfs
    // cannot serve them. Pass an empty src list for now; the build will be
    // wired up separately when thirdparty sandboxing is implemented.
    target_lib::build_spec(
        addr,
        &pkg.import_path,
        pkg.name.as_deref().unwrap_or(""),
        factors,
        transitive_libs,
        &[],
        go_bin_addr,
        goroot,
        None,
    )
}
