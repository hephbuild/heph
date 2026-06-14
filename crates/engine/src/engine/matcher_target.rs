//! Evaluate a [`Matcher`] against a fully-parsed [`TargetDef`].
//!
//! This lives in the engine (not `htmatcher`) because it is the one place the
//! pure matcher predicate meets the target data model: `TreeOutputTo` needs the
//! target's output paths and their `codegen_tree` modes, which only exist after
//! `Driver::parse`. Keeping it here lets `htmatcher` stay a leaf with no
//! dependency on the driver/target-def types.

use crate::engine::driver::targetdef::TargetDef;
use hmodel::htmatcher::{MatchResult, Matcher};
use hmodel::htpkg::PkgBuf;

/// Normalise a relative filesystem path to a package path. Mirrors
/// `tref.ToPackage` — strips leading `./`, treats `.` as the empty package.
/// Engine output paths are already `/`-separated, so no separator translation.
fn path_to_pkg(s: &str) -> String {
    if s == "." || s.is_empty() {
        return String::new();
    }
    let mut s = s;
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.trim_end_matches('/').to_string()
}

/// Return the directory portion of `p` (everything before the last `/`),
/// or `""` if no separator exists.
fn parent_dir(p: &str) -> &str {
    p.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("")
}

/// Split a glob pattern into `(base, pattern)` where `base` is the longest
/// `/`-bounded prefix containing no glob metacharacters. Mirrors
/// `hfs.GlobSplit` for the cases relevant to the matcher. `/` and the glob
/// metas (`*`, `?`, `[`, `{`) are all ASCII, so `seg_start` and `base_end`
/// always land on a UTF-8 char boundary.
#[expect(
    clippy::string_slice,
    reason = "indices come from ASCII matches only — always char-aligned"
)]
fn glob_split(g: &str) -> (&str, &str) {
    let bytes = g.as_bytes();
    let mut seg_start = 0;
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'/' {
            seg_start = i + 1;
            continue;
        }
        if matches!(c, b'*' | b'?' | b'[' | b'{') {
            let base_end = seg_start.saturating_sub(1);
            return (&g[..base_end], &g[seg_start..]);
        }
    }
    (g, "")
}

/// Resolve a matcher against a parsed target definition.
pub fn match_target(m: &Matcher, def: &TargetDef) -> MatchResult {
    match m {
        Matcher::Addr(addr) => {
            if def.addr == *addr {
                MatchResult::MatchYes
            } else {
                MatchResult::MatchNo
            }
        }
        Matcher::Label(label) => {
            if def.labels.contains(label) {
                MatchResult::MatchYes
            } else {
                MatchResult::MatchNo
            }
        }
        Matcher::TreeOutputTo(matcher_pkg) => {
            use crate::engine::driver::targetdef::path::{CodegenMode, Content};

            // Cheap reject (same shape as matches_addr): a codegen output
            // of a target at `def_pkg` lands at `def_pkg/<rel>`. If
            // neither package is a prefix of the other, no output can
            // possibly land under (or contain) the matcher package.
            if !def.addr.package.has_prefix(matcher_pkg)
                && !matcher_pkg.has_prefix(&def.addr.package)
            {
                return MatchResult::MatchNo;
            }

            // Output paths produced by `Driver::parse` (e.g. pluginexec
            // via `spec_path_to_target_path`) are already package-rooted
            // — the target's package prefix has been prepended. So we
            // read the output package straight from the path, no join
            // with `def.addr.package` needed.
            for output in &def.outputs {
                for path in &output.paths {
                    if matches!(path.codegen_tree, CodegenMode::None) {
                        continue;
                    }
                    let (out_pkg, accept_either) = match &path.content {
                        Content::FilePath(p) => {
                            // Single file: pkg is its directory.
                            (path_to_pkg(parent_dir(p)), false)
                        }
                        Content::DirPath(p) => (path_to_pkg(p), true),
                        Content::Glob(g) => {
                            let (base, _) = glob_split(g);
                            (path_to_pkg(base), true)
                        }
                    };
                    let out_pkg = PkgBuf::from(out_pkg);
                    let matched = if accept_either {
                        out_pkg.has_prefix(matcher_pkg) || matcher_pkg.has_prefix(&out_pkg)
                    } else {
                        out_pkg.has_prefix(matcher_pkg)
                    };
                    if matched {
                        return MatchResult::MatchYes;
                    }
                }
            }
            MatchResult::MatchNo
        }
        Matcher::Package(pkg) => {
            if def.addr.package == *pkg {
                MatchResult::MatchYes
            } else {
                MatchResult::MatchNo
            }
        }
        Matcher::PackagePrefix(prefix) => {
            if def.addr.package.has_prefix(prefix) {
                MatchResult::MatchYes
            } else {
                MatchResult::MatchNo
            }
        }
        Matcher::Or(matchers) => {
            let mut has_shrug = false;
            for m in matchers {
                match match_target(m, def) {
                    MatchResult::MatchYes => return MatchResult::MatchYes,
                    MatchResult::MatchShrug => has_shrug = true,
                    MatchResult::MatchNo => {}
                }
            }
            if has_shrug {
                MatchResult::MatchShrug
            } else {
                MatchResult::MatchNo
            }
        }
        Matcher::And(matchers) => {
            let mut has_shrug = false;
            for m in matchers {
                match match_target(m, def) {
                    MatchResult::MatchNo => return MatchResult::MatchNo,
                    MatchResult::MatchShrug => has_shrug = true,
                    MatchResult::MatchYes => {}
                }
            }
            if has_shrug {
                MatchResult::MatchShrug
            } else {
                MatchResult::MatchYes
            }
        }
        Matcher::Not(m) => match match_target(m, def) {
            MatchResult::MatchYes => MatchResult::MatchNo,
            MatchResult::MatchNo => MatchResult::MatchYes,
            MatchResult::MatchShrug => MatchResult::MatchShrug,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::targetdef::path::Path;
    use hmodel::htaddr::Addr;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn addr(pkg: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
    }

    fn def_with_labels(pkg: &str, name: &str, labels: &[&str]) -> TargetDef {
        TargetDef {
            addr: addr(pkg, name),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            raw_def: Arc::new(()),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: crate::engine::driver::targetdef::CacheConfig::off(),
            pty: false,
            hash: vec![],
            transparent: false,
        }
    }

    fn def(pkg: &str, name: &str) -> TargetDef {
        def_with_labels(pkg, name, &[])
    }

    #[test]
    fn path_to_pkg_handles_dot_and_relative_prefix() {
        assert_eq!(path_to_pkg("."), "");
        assert_eq!(path_to_pkg(""), "");
        assert_eq!(path_to_pkg("./foo/bar"), "foo/bar");
        assert_eq!(path_to_pkg("foo/bar"), "foo/bar");
        assert_eq!(path_to_pkg("foo/bar/"), "foo/bar");
    }

    #[test]
    fn parent_dir_works() {
        assert_eq!(parent_dir("foo/bar/baz.go"), "foo/bar");
        assert_eq!(parent_dir("foo.go"), "");
        assert_eq!(parent_dir(""), "");
        assert_eq!(parent_dir("a/b"), "a");
    }

    #[test]
    fn glob_split_separates_base_and_pattern() {
        assert_eq!(glob_split("cmd/gen/*.go"), ("cmd/gen", "*.go"));
        assert_eq!(glob_split("cmd/**/*"), ("cmd", "**/*"));
        assert_eq!(glob_split("*"), ("", "*"));
        assert_eq!(glob_split("foo/bar"), ("foo/bar", ""));
        assert_eq!(glob_split("foo/[abc]/bar"), ("foo", "[abc]/bar"));
    }

    #[test]
    fn def_addr_match() {
        let d = def("foo/bar", "build");
        assert_eq!(
            match_target(&Matcher::Addr(addr("foo/bar", "build")), &d),
            MatchResult::MatchYes
        );
        assert_eq!(
            match_target(&Matcher::Addr(addr("foo/bar", "test")), &d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn def_label_match() {
        let d = def_with_labels("foo", "bar", &["//labels:lint"]);
        assert_eq!(
            match_target(&Matcher::Label("//labels:lint".to_string()), &d),
            MatchResult::MatchYes
        );
        assert_eq!(
            match_target(&Matcher::Label("//labels:other".to_string()), &d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn def_package_match() {
        let d = def("foo/bar", "build");
        assert_eq!(
            match_target(&Matcher::Package(PkgBuf::from("foo/bar")), &d),
            MatchResult::MatchYes
        );
        assert_eq!(
            match_target(&Matcher::Package(PkgBuf::from("foo")), &d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn def_package_prefix_match() {
        let d = def("foo/bar/baz", "build");
        assert_eq!(
            match_target(&Matcher::PackagePrefix(PkgBuf::from("foo/bar")), &d),
            MatchResult::MatchYes
        );
        assert_eq!(
            match_target(&Matcher::PackagePrefix(PkgBuf::from("other")), &d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn def_or_match() {
        let d = def("foo", "bar");
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("other")),
            Matcher::Package(PkgBuf::from("foo")),
        ]);
        assert_eq!(match_target(&m, &d), MatchResult::MatchYes);
    }

    #[test]
    fn def_and_match() {
        let d = def_with_labels("foo", "bar", &["//labels:lint"]);
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo")),
            Matcher::Label("//labels:lint".to_string()),
        ]);
        assert_eq!(match_target(&m, &d), MatchResult::MatchYes);
        let m2 = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo")),
            Matcher::Label("//labels:other".to_string()),
        ]);
        assert_eq!(match_target(&m2, &d), MatchResult::MatchNo);
    }

    #[test]
    fn def_not_match() {
        let d = def("foo", "bar");
        assert_eq!(
            match_target(
                &Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("other")))),
                &d
            ),
            MatchResult::MatchYes
        );
        assert_eq!(
            match_target(
                &Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("foo")))),
                &d
            ),
            MatchResult::MatchNo
        );
    }

    fn def_with_paths(pkg: &str, name: &str, paths: Vec<Path>) -> TargetDef {
        use crate::engine::driver::targetdef::Output;
        TargetDef {
            addr: addr(pkg, name),
            labels: vec![],
            raw_def: Arc::new(()),
            inputs: vec![],
            outputs: vec![Output {
                group: "out".to_string(),
                paths,
            }],
            support_files: vec![],
            cache: crate::engine::driver::targetdef::CacheConfig::off(),
            pty: false,
            hash: vec![],
            transparent: false,
        }
    }

    fn p_dir(s: &str, codegen: bool) -> Path {
        use crate::engine::driver::targetdef::path::{CodegenMode, Content};
        Path {
            content: Content::DirPath(s.to_string()),
            codegen_tree: if codegen {
                CodegenMode::Copy
            } else {
                CodegenMode::None
            },
            collect: false,
        }
    }

    fn p_file(s: &str, codegen: bool) -> Path {
        use crate::engine::driver::targetdef::path::{CodegenMode, Content};
        Path {
            content: Content::FilePath(s.to_string()),
            codegen_tree: if codegen {
                CodegenMode::Copy
            } else {
                CodegenMode::None
            },
            collect: false,
        }
    }

    fn p_glob(s: &str, codegen: bool) -> Path {
        use crate::engine::driver::targetdef::path::{CodegenMode, Content};
        Path {
            content: Content::Glob(s.to_string()),
            codegen_tree: if codegen {
                CodegenMode::Copy
            } else {
                CodegenMode::None
            },
            collect: false,
        }
    }

    #[test]
    fn tree_output_to_yes_when_dir_output_in_pkg() {
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", true)]);
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("foo/gen")), &d),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn tree_output_to_yes_when_matcher_is_inside_dir_output() {
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", true)]);
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("foo/gen/sub")), &d),
            MatchResult::MatchYes,
        );
    }

    #[test]
    fn tree_output_to_no_when_codegen_disabled() {
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", false)]);
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("foo/gen")), &d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn tree_output_to_no_when_output_in_different_pkg() {
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", true)]);
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("bar")), &d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn tree_output_to_file_output_strict_prefix_only() {
        let d = def_with_paths("foo", "bar", vec![p_file("foo/gen/file.go", true)]);
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("foo/gen")), &d),
            MatchResult::MatchYes
        );
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("foo/gen/deeper")), &d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn tree_output_to_glob_uses_glob_base() {
        let d = def_with_paths("foo", "bar", vec![p_glob("foo/gen/**/*.go", true)]);
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("foo/gen")), &d),
            MatchResult::MatchYes
        );
        assert_eq!(
            match_target(&Matcher::TreeOutputTo(PkgBuf::from("foo/gen/inner")), &d),
            MatchResult::MatchYes
        );
    }
}
