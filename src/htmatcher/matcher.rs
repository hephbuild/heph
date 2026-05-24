use crate::engine::driver::targetdef::TargetDef;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MatchResult {
    MatchYes,
    MatchNo,
    MatchShrug,
}

/// Matcher predicates over a target. `TreeOutputTo(pkg)` selects targets whose
/// codegen-tree outputs land inside (or contain) the package `pkg` — mirrors
/// heph's `CodegenPackage` (see `internal/tmatch/match.go::MatchDef`). The
/// argument is a package path (e.g. `cmd/foo/gen`), **not** an output group
/// name. Resolution requires the target's `def` because the output paths and
/// their `codegen_tree` modes are only known after `Driver::parse`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Matcher {
    Addr(Addr),
    Label(String),
    Package(PkgBuf),
    PackagePrefix(PkgBuf),
    TreeOutputTo(PkgBuf),
    Or(Vec<Matcher>),
    And(Vec<Matcher>),
    Not(Box<Matcher>),
}

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

#[cfg(test)]
mod path_helper_tests {
    use super::*;

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
}

impl Matcher {
    pub fn matches_addr(&self, addr: &Addr) -> MatchResult {
        match self {
            Matcher::Addr(a) => {
                if a == addr {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Label(_) => MatchResult::MatchShrug,
            Matcher::TreeOutputTo(matcher_pkg) => {
                // Cheap addr-only reject: the codegen tree of a target at pkg
                // `def_pkg` lands under `def_pkg`, so the matcher's package
                // and the target's package must lie on the same root path.
                // If neither is a prefix of the other, no output can match.
                // Otherwise we need the def to inspect output paths.
                if addr.package.has_prefix(matcher_pkg) || matcher_pkg.has_prefix(&addr.package) {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Package(pkg) => {
                if &addr.package == pkg {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::PackagePrefix(prefix) => {
                if addr.package.has_prefix(prefix) {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Or(matchers) => {
                let mut shrug = false;
                for m in matchers {
                    match m.matches_addr(addr) {
                        MatchResult::MatchYes => return MatchResult::MatchYes,
                        MatchResult::MatchShrug => shrug = true,
                        MatchResult::MatchNo => {}
                    }
                }
                if shrug {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::And(matchers) => {
                let mut shrug = false;
                for m in matchers {
                    match m.matches_addr(addr) {
                        MatchResult::MatchNo => return MatchResult::MatchNo,
                        MatchResult::MatchShrug => shrug = true,
                        MatchResult::MatchYes => {}
                    }
                }
                if shrug {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchYes
                }
            }
            Matcher::Not(m) => match m.matches_addr(addr) {
                MatchResult::MatchYes => MatchResult::MatchNo,
                MatchResult::MatchNo => MatchResult::MatchYes,
                MatchResult::MatchShrug => MatchResult::MatchShrug,
            },
        }
    }

    pub fn matches(&self, def: &TargetDef) -> MatchResult {
        match self {
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
                    match m.matches(def) {
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
                    match m.matches(def) {
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
            Matcher::Not(m) => match m.matches(def) {
                MatchResult::MatchYes => MatchResult::MatchNo,
                MatchResult::MatchNo => MatchResult::MatchYes,
                MatchResult::MatchShrug => MatchResult::MatchShrug,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn addr(pkg: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
    }

    fn def_with_labels(pkg: &str, name: &str, labels: &[&str]) -> TargetDef {
        use std::sync::Arc;
        TargetDef {
            addr: addr(pkg, name),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            raw_def: Arc::new(()),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: false,
            disable_remote_cache: false,
            pty: false,
            hash: vec![],
            transparent: false,
        }
    }

    fn def(pkg: &str, name: &str) -> TargetDef {
        def_with_labels(pkg, name, &[])
    }

    // matches_addr tests

    #[test]
    fn addr_exact_match() {
        let a = addr("foo/bar", "baz");
        assert_eq!(
            Matcher::Addr(a.clone()).matches_addr(&a),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn addr_no_match() {
        let a = addr("foo/bar", "baz");
        let b = addr("foo/bar", "qux");
        assert_eq!(Matcher::Addr(a).matches_addr(&b), MatchResult::MatchNo);
    }

    #[test]
    fn label_always_shrugs() {
        let a = addr("foo/bar", "baz");
        assert_eq!(
            Matcher::Label("my_label".to_string()).matches_addr(&a),
            MatchResult::MatchShrug
        );
    }

    #[test]
    fn package_match() {
        let a = addr("foo/bar", "baz");
        assert_eq!(
            Matcher::Package(PkgBuf::from("foo/bar")).matches_addr(&a),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::Package(PkgBuf::from("foo")).matches_addr(&a),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn package_prefix_match() {
        let a = addr("foo/bar/baz", "t");
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("foo/bar")).matches_addr(&a),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("foo/ba")).matches_addr(&a),
            MatchResult::MatchNo
        );
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("")).matches_addr(&a),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn or_yes_if_any_yes() {
        let a = addr("foo/bar", "t");
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("other")),
            Matcher::Package(PkgBuf::from("foo/bar")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchYes);
    }

    #[test]
    fn or_no_if_all_no() {
        let a = addr("foo/bar", "t");
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("a")),
            Matcher::Package(PkgBuf::from("b")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchNo);
    }

    #[test]
    fn or_shrug_if_no_yes_but_some_shrug() {
        let a = addr("foo/bar", "t");
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("other")),
            Matcher::Label("lbl".to_string()),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchShrug);
    }

    #[test]
    fn and_yes_if_all_yes() {
        let a = addr("foo/bar", "t");
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::PackagePrefix(PkgBuf::from("foo")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchYes);
    }

    #[test]
    fn and_no_if_any_no() {
        let a = addr("foo/bar", "t");
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::Package(PkgBuf::from("other")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchNo);
    }

    #[test]
    fn and_shrug_if_no_no_but_some_shrug() {
        let a = addr("foo/bar", "t");
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::Label("lbl".to_string()),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchShrug);
    }

    #[test]
    fn not_flips() {
        let a = addr("foo/bar", "t");
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("foo/bar")))).matches_addr(&a),
            MatchResult::MatchNo
        );
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("other")))).matches_addr(&a),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn not_shrug_stays_shrug() {
        let a = addr("foo/bar", "t");
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Label("lbl".to_string()))).matches_addr(&a),
            MatchResult::MatchShrug
        );
    }

    // matches (TargetDef) tests

    #[test]
    fn def_addr_match() {
        let d = def("foo/bar", "build");
        assert_eq!(
            Matcher::Addr(addr("foo/bar", "build")).matches(&d),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::Addr(addr("foo/bar", "test")).matches(&d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn def_label_match() {
        let d = def_with_labels("foo", "bar", &["//labels:lint"]);
        assert_eq!(
            Matcher::Label("//labels:lint".to_string()).matches(&d),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::Label("//labels:other".to_string()).matches(&d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn def_package_match() {
        let d = def("foo/bar", "build");
        assert_eq!(
            Matcher::Package(PkgBuf::from("foo/bar")).matches(&d),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::Package(PkgBuf::from("foo")).matches(&d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn def_package_prefix_match() {
        let d = def("foo/bar/baz", "build");
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("foo/bar")).matches(&d),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("other")).matches(&d),
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
        assert_eq!(m.matches(&d), MatchResult::MatchYes);
    }

    #[test]
    fn def_and_match() {
        let d = def_with_labels("foo", "bar", &["//labels:lint"]);
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo")),
            Matcher::Label("//labels:lint".to_string()),
        ]);
        assert_eq!(m.matches(&d), MatchResult::MatchYes);
        let m2 = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo")),
            Matcher::Label("//labels:other".to_string()),
        ]);
        assert_eq!(m2.matches(&d), MatchResult::MatchNo);
    }

    #[test]
    fn exclude_composition_drops_specific_addr_in_package() {
        // Mirrors the CLI -e contract: `And([Package(p), Not(Addr(a))])`
        // includes targets in p but excludes a specifically.
        let pkg = PkgBuf::from("foo");
        let bad = addr("foo", "bad");
        let good = addr("foo", "good");
        let outside = addr("other", "good");

        let m = Matcher::And(vec![
            Matcher::Package(pkg),
            Matcher::Not(Box::new(Matcher::Addr(bad.clone()))),
        ]);

        assert_eq!(m.matches_addr(&bad), MatchResult::MatchNo);
        assert_eq!(m.matches_addr(&good), MatchResult::MatchYes);
        assert_eq!(m.matches_addr(&outside), MatchResult::MatchNo);
    }

    #[test]
    fn def_not_match() {
        let d = def("foo", "bar");
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("other")))).matches(&d),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("foo")))).matches(&d),
            MatchResult::MatchNo
        );
    }

    fn def_with_paths(
        pkg: &str,
        name: &str,
        paths: Vec<crate::engine::driver::targetdef::path::Path>,
    ) -> TargetDef {
        use crate::engine::driver::targetdef::Output;
        use std::sync::Arc;
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
            cache: false,
            disable_remote_cache: false,
            pty: false,
            hash: vec![],
            transparent: false,
        }
    }

    fn p_dir(s: &str, codegen: bool) -> crate::engine::driver::targetdef::path::Path {
        use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
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

    fn p_file(s: &str, codegen: bool) -> crate::engine::driver::targetdef::path::Path {
        use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
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

    fn p_glob(s: &str, codegen: bool) -> crate::engine::driver::targetdef::path::Path {
        use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
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
    fn tree_output_to_addr_shrugs_when_packages_overlap() {
        // matcher pkg `foo/gen` and target at pkg `foo` may or may not match
        // — need def to inspect outputs.
        let a = addr("foo", "bar");
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen")).matches_addr(&a),
            MatchResult::MatchShrug
        );
        // matcher pkg under target's pkg: also possible.
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo")).matches_addr(&a),
            MatchResult::MatchShrug
        );
    }

    #[test]
    fn tree_output_to_addr_no_when_packages_unrelated() {
        // matcher pkg `bar` cannot be reached by codegen of target at `foo`.
        let a = addr("foo", "bar");
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("bar")).matches_addr(&a),
            MatchResult::MatchNo
        );
    }

    // Note: `Driver::parse` (e.g. pluginexec::spec_path_to_target_path) prepends
    // the target's package to each output path, so test fixtures pass paths
    // that already include the pkg prefix.

    #[test]
    fn tree_output_to_yes_when_dir_output_in_pkg() {
        // Target at `foo` with codegen DirPath `foo/gen` → matcher `foo/gen`.
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", true)]);
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen")).matches(&d),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn tree_output_to_yes_when_matcher_is_inside_dir_output() {
        // DirPath output covers the matcher pkg → accept either way.
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", true)]);
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen/sub")).matches(&d),
            MatchResult::MatchYes,
        );
    }

    #[test]
    fn tree_output_to_no_when_codegen_disabled() {
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", false)]);
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen")).matches(&d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn tree_output_to_no_when_output_in_different_pkg() {
        // Target at `foo` with codegen DirPath `foo/gen`. Matcher `bar` is
        // unrelated and gets rejected by the cheap addr-level check.
        let d = def_with_paths("foo", "bar", vec![p_dir("foo/gen", true)]);
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("bar")).matches(&d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn tree_output_to_file_output_strict_prefix_only() {
        // FilePath cannot contain the matcher pkg — only `out_pkg.has_prefix(matcher)`.
        // Output `foo/gen/file.go` → dirname `foo/gen`.
        let d = def_with_paths("foo", "bar", vec![p_file("foo/gen/file.go", true)]);
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen")).matches(&d),
            MatchResult::MatchYes
        );
        // Matcher under the file's pkg — file does not cover a subtree.
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen/deeper")).matches(&d),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn tree_output_to_glob_uses_glob_base() {
        // Glob `foo/gen/**/*.go` → base `foo/gen`.
        let d = def_with_paths("foo", "bar", vec![p_glob("foo/gen/**/*.go", true)]);
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen")).matches(&d),
            MatchResult::MatchYes
        );
        // Matcher deeper than glob base — glob's subtree covers it.
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen/inner")).matches(&d),
            MatchResult::MatchYes
        );
    }
}
