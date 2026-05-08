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
    src_addr: &Addr,
    go_bin_addr: &str,
    goroot: &str,
) -> TargetSpec {
    target_lib::build_spec(
        addr,
        &pkg.import_path,
        pkg.name.as_deref().unwrap_or(""),
        factors,
        transitive_libs,
        src_addr,
        go_bin_addr,
        goroot,
        None,
    )
}
