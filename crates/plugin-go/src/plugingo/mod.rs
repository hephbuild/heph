mod addr_util;
mod cc_toolchain;
mod driver_compile;
mod driver_format;
mod driver_golist;
mod driver_lint;
mod driver_testmain;
mod embed;
pub(crate) mod errors;
mod factors;
mod gen_testmain;
pub mod gocache;
mod govet;
mod pkg_analysis;
mod provider;
mod target_bin;
mod target_golist;
mod target_lib;
mod target_modfiles;
mod target_std;
mod target_test;
mod thirdparty;
mod toolchain;
mod variant;

pub use driver_compile::GoCompileDriver;
pub use driver_format::{GoFormatCheckDriver, GoFormatDriver};
pub use driver_golist::GoGolistDriver;
pub use driver_lint::{GoLintDriver, GoLintFixDriver, GoLintGateDriver};
pub use driver_testmain::GoTestmainDriver;
pub use provider::{Config, Provider};
// `DEFAULT_GO_VERSION` / `HOST` / `checksum_key` are re-exported for the
// `plugingo-e2e` harness: it must pin the same toolchain this provider does and
// render `checksums` keys exactly as the driver looks them up. It used to spell
// its own copies, which is a silent-failure shape — a checksum key that matches
// nothing is not an error, it downloads the SDK unverified (the driver only
// warns), so a drifted copy leaves the suite green while it tests an unverified
// toolchain.
pub use toolchain::{DEFAULT_GO_VERSION, GoToolchainDriver, HOST, checksum_key};

pub mod runner;
