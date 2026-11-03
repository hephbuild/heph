mod addr_util;
mod cc_toolchain;
mod driver_compile;
mod driver_download;
mod driver_format;
mod driver_golist;
mod driver_headers;
mod driver_lint;
mod driver_testmain;
mod embed;
pub(crate) mod errors;
mod factors;
mod gen_testmain;
pub mod gocache;
mod govet;
mod listed;
mod module_fetch;
mod nongo_lanes;
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
pub use driver_download::GoDownloadDriver;
pub use driver_format::{GoFormatCheckDriver, GoFormatDriver};
pub use driver_golist::GoGolistDriver;
// `extract_go_build_header` is public for the plugingo-e2e differential test,
// which holds it to the real `go/build` on every supported Go.
pub use driver_headers::{GoHeadersDriver, extract_go_build_header};
pub use driver_lint::{GoLintDriver, GoLintFixDriver, GoLintGateDriver};
pub use driver_testmain::GoTestmainDriver;
// The D4 rules, for the plugingo-e2e differentials (per-extension body
// mutation, extractor vs `go/build`) to iterate over the same set.
pub use nongo_lanes::{
    GO_HEADER_READ_EXTS, GO_HEADER_RULES_VERIFIED_AGAINST, NonGoLane, classify_non_go,
};
// `_golist`'s `package.bin`, for the plugingo-e2e tests that assert what `go list`
// reported (`EmbedFiles`, `SFiles`, the package error) after a workspace edit.
pub use pkg_analysis::{GoPackage, decode_go_package};
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
