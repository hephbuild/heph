use std::fmt;

/// Returned by `read_golist_package` when `go list -e` reports the package has
/// no buildable Go files (`go_files`/`test_go_files`/`xtest_go_files` all empty
/// and an `error` field set by go list — typically "no Go files in <dir>" or
/// "build constraints exclude all Go files in <dir>").
///
/// Mirrors `errNoGoFiles` in
/// `/Users/rvigee/Documents/Code/heph/plugin/plugingo/pkg_analysis.go:34`.
/// Callers detect it via `hmemoizer::downcast_chain_ref::<NoGoFilesError>` to
/// short-circuit to `GetError::NotFound`.
#[derive(Debug, Clone)]
pub struct NoGoFilesError {
    pub import_path: String,
}

impl fmt::Display for NoGoFilesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no Go files in package {}", self.import_path)
    }
}

impl std::error::Error for NoGoFilesError {}
