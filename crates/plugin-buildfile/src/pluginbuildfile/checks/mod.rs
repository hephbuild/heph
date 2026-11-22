//! The BUILD-file checks: `build-lint-check`, `build-format-check` and
//! `build-format` (docs/BUILDFILE_CHECKS.md).
//!
//! - [`loads`]: the parse-only load walk (I8) and the ownership rule (I1).
//! - [`annotations`]: the `requireTypeAnnotations` rule, shared with the LSP.
//! - [`lint`]: the static typecheck over a walk, without executing (I4).
//! - [`format`]: the format check and fix, on `xstarlark-fmt`'s one rule.
//! - [`report`]: findings and the failure text, which is the contract.
//! - [`targets`]: the three targets' names, labels, drivers and specs.
//! - [`drivers`]: the three drivers that run them.

pub(crate) mod annotations;
pub(crate) mod drivers;
pub(crate) mod format;
pub(crate) mod lint;
pub(crate) mod loads;
pub(crate) mod report;
pub(crate) mod targets;

#[cfg(test)]
mod load_edges;
#[cfg(test)]
mod test_ws;

#[cfg(test)]
use test_ws::{assert_matches_eval, walk, ws};

/// Differential (I8): over a fixture table, the files the walk resolves are
/// exactly the files evaluation's loader opens, and the two fail on the same
/// inputs with the same message.
#[cfg(test)]
#[test]
fn load_walk_matches_eval() {
    type Fixture = (
        &'static str,
        &'static [(&'static str, &'static str)],
        &'static str,
    );
    let fixtures: &[Fixture] = &[
        ("no loads", &[("a/BUILD", "X = 1\n")], "a"),
        (
            "file load",
            &[
                ("a/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
                ("lib/defs.star", "X = 1\n"),
            ],
            "a",
        ),
        (
            "dir load, every pattern file",
            &[
                ("a/BUILD", "load(\"//lib\", \"X\")\n"),
                ("lib/BUILD", "X = 1\n"),
                ("lib/more.BUILD", "Y = 2\n"),
                ("lib/notes.txt", "not starlark ((\n"),
            ],
            "a",
        ),
        (
            "package with a helper",
            &[
                ("a/BUILD", "load(\"./defs.star\", \"X\")\nY = X\n"),
                ("a/helper.BUILD", "load(\"//lib/defs.star\", \"Z\")\n"),
                ("a/defs.star", "X = 1\n"),
                ("lib/defs.star", "Z = 1\n"),
            ],
            "a",
        ),
        (
            "diamond",
            &[
                (
                    "a/BUILD",
                    "load(\"//b/b.star\", \"B\")\nload(\"//c/c.star\", \"C\")\n",
                ),
                ("b/b.star", "load(\"//d/d.star\", \"D\")\nB = D\n"),
                ("c/c.star", "load(\"//d/d.star\", \"D\")\nC = D\n"),
                ("d/d.star", "D = 1\n"),
            ],
            "a",
        ),
        (
            "depth 2",
            &[
                ("a/BUILD", "load(\"//b/b.star\", \"B\")\n"),
                ("b/b.star", "load(\"//c/c.star\", \"C\")\nB = C\n"),
                ("c/c.star", "C = 1\n"),
            ],
            "a",
        ),
        (
            "relative load in a loaded file",
            &[
                ("a/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
                ("lib/defs.star", "load(\"./more.star\", \"Y\")\nX = Y\n"),
                ("lib/more.star", "Y = 1\n"),
            ],
            "a",
        ),
        (
            "relative load in a dir-loaded file",
            &[
                ("a/BUILD", "load(\"//lib\", \"X\")\n"),
                ("lib/BUILD", "load(\"./more.star\", \"Y\")\nX = Y\n"),
                ("lib/more.star", "Y = 1\n"),
            ],
            "a",
        ),
        (
            "trailing slash",
            &[
                ("a/BUILD", "load(\"//lib/defs.star/\", \"X\")\n"),
                ("lib/defs.star", "X = 1\n"),
            ],
            "a",
        ),
        (
            "parent-relative load",
            &[
                ("a/b/BUILD", "load(\"../defs.star\", \"X\")\n"),
                ("a/defs.star", "X = 1\n"),
            ],
            "a/b",
        ),
        (
            "file cycle",
            &[
                ("a/BUILD", "load(\"./x.star\", \"X\")\n"),
                ("a/x.star", "load(\"./y.star\", \"Y\")\nX = 1\n"),
                ("a/y.star", "load(\"./x.star\", \"X\")\nY = 1\n"),
            ],
            "a",
        ),
        (
            "self-load",
            &[("a/BUILD", "load(\"./BUILD\", \"X\")\nX = 1\n")],
            "a",
        ),
        (
            "cross-package cycle",
            &[
                ("a/BUILD", "load(\"//b\", \"Y\")\nX = 1\n"),
                ("b/BUILD", "load(\"//a\", \"X\")\nY = 1\n"),
            ],
            "a",
        ),
        (
            "missing file",
            &[("a/BUILD", "load(\"//lib/nope.star\", \"X\")\n")],
            "a",
        ),
        (
            "escape",
            &[("a/BUILD", "load(\"../../../etc/hosts\", \"X\")\n")],
            "a",
        ),
        (
            "bare path",
            &[("a/BUILD", "load(\"defs.star\", \"X\")\n")],
            "a",
        ),
        (
            "slashes alone",
            &[("a/BUILD", "load(\"//\", \"X\")\n")],
            "a",
        ),
        (
            "dir with no BUILD file",
            &[
                ("a/BUILD", "load(\"//empty\", \"X\")\n"),
                ("empty/defs.star", "X = 1\n"),
            ],
            "a",
        ),
        (
            "parse error in a loaded file",
            &[
                ("a/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
                ("lib/defs.star", "X = (\n"),
            ],
            "a",
        ),
    ];
    for (name, files, pkg) in fixtures {
        let tmp = ws(files);
        assert_matches_eval(tmp.path(), pkg, name);
    }

    // A symlinked loaded file keeps its link path, and its link's package.
    let tmp = ws(&[
        ("a/BUILD", "load(\"./link.star\", \"X\")\n"),
        ("lib/real.star", "X = 1\n"),
    ]);
    std::os::unix::fs::symlink("../lib/real.star", tmp.path().join("a/link.star"))
        .expect("symlink");
    assert_matches_eval(tmp.path(), "a", "symlinked file");
}

/// A `./` load resolves against the package of the file it is written in: a
/// file loaded across packages, and a file loaded through a directory load,
/// each use their own directory, never the package that loaded them.
#[cfg(test)]
#[test]
fn relative_load_in_loaded_file() {
    use loads::LoadEdge;
    let tmp = ws(&[
        (
            "a/BUILD",
            "load(\"//lib/defs.star\", \"X\")\nload(\"//dirlib\", \"Y\")\n",
        ),
        // Decoys: a wrong resolution against `a` would find these.
        ("a/more.star", "M = 0\n"),
        ("a/helper.star", "H = 0\n"),
        ("lib/defs.star", "load(\"./more.star\", \"M\")\nX = M\n"),
        ("lib/more.star", "M = 1\n"),
        ("dirlib/BUILD", "load(\"./helper.star\", \"H\")\nY = H\n"),
        ("dirlib/helper.star", "H = 1\n"),
    ]);
    let walk = walk(tmp.path(), "a").expect("walk");
    assert_eq!(
        walk.files["lib/defs.star"].loads["./more.star"],
        LoadEdge::File {
            path: "lib/more.star".into(),
            pkg: "lib".into()
        }
    );
    let dirlib = &walk.files["a/BUILD"].loads["//dirlib"];
    assert_eq!(dirlib.pkg(), "dirlib");
    assert_eq!(dirlib.files().collect::<Vec<_>>(), ["dirlib/BUILD"]);
    assert_eq!(walk.files["dirlib/BUILD"].pkg, "dirlib");
    assert_eq!(
        walk.files["dirlib/BUILD"].loads["./helper.star"],
        LoadEdge::File {
            path: "dirlib/helper.star".into(),
            pkg: "dirlib".into()
        }
    );
    assert!(!walk.files.contains_key("a/more.star"));
    assert!(!walk.files.contains_key("a/helper.star"));
    assert_matches_eval(tmp.path(), "a", "relative loads");
}

/// I1: a package owns its BUILD files, its `*.BUILD` helpers and the files of
/// its own directory it loads; a sibling's file it loads is an input, not
/// owned. A sibling loading the helper does not take it over.
#[cfg(test)]
#[test]
fn ownership() {
    let tmp = ws(&[
        (
            "x/BUILD",
            "load(\"./lib.star\", \"L\")\nload(\"//y/lib.star\", \"YL\")\n",
        ),
        ("x/defs.BUILD", "D = 1\n"),
        ("x/lib.star", "L = 1\n"),
        // In x's directory but loaded by nobody: reached by no walk (E1).
        ("x/unloaded.star", "U = 1\n"),
        (
            "y/BUILD",
            "load(\"//x/defs.BUILD\", \"D\")\nload(\"./lib.star\", \"YL\")\n",
        ),
        ("y/lib.star", "YL = 1\n"),
    ]);
    let collect = |it: &mut dyn Iterator<Item = &str>| it.map(str::to_string).collect::<Vec<_>>();

    let x = walk(tmp.path(), "x").expect("walk x");
    assert_eq!(
        collect(&mut x.owned()),
        ["x/BUILD", "x/defs.BUILD", "x/lib.star"]
    );
    assert_eq!(
        collect(&mut x.inputs()),
        ["x/BUILD", "x/defs.BUILD", "x/lib.star", "y/lib.star"]
    );

    let y = walk(tmp.path(), "y").expect("walk y");
    assert_eq!(collect(&mut y.owned()), ["y/BUILD", "y/lib.star"]);
    assert_eq!(
        collect(&mut y.inputs()),
        ["x/defs.BUILD", "y/BUILD", "y/lib.star"]
    );
}

/// I1: a symlinked file is never owned, even in the package's own directory:
/// `build-format` neither reads nor writes it (a rewrite would land on the
/// link's target, which another package may own), and lint discards its
/// findings. Its interface still types the package's calls into it.
#[cfg(test)]
#[test]
fn symlinked_file_not_owned() {
    use targets::{CheckKind, ChecksOptions, format_spec};
    let tmp = ws(&[
        (
            "pkg/BUILD",
            "load(\"./helper.star\", \"make\")\n\ndef ok():\n    make(1)\n\n\
             def bad():\n    make(\"s\")\n",
        ),
        (
            "lib/real.star",
            "def make(n: int) -> None:\n    own: int = \"s\"\n",
        ),
    ]);
    std::os::unix::fs::symlink("../lib/real.star", tmp.path().join("pkg/helper.star"))
        .expect("symlink");

    let walk = walk(tmp.path(), "pkg").expect("walk");
    assert!(walk.files["pkg/helper.star"].symlink);
    assert_eq!(walk.files["pkg/helper.star"].pkg, "pkg");
    assert_eq!(walk.owned().collect::<Vec<_>>(), ["pkg/BUILD"]);
    assert_eq!(
        walk.inputs().collect::<Vec<_>>(),
        ["pkg/BUILD", "pkg/helper.star"]
    );

    let addr = hmodel::htaddr::Addr::new(
        hmodel::htpkg::PkgBuf::from("pkg"),
        CheckKind::Format.name().to_string(),
        Default::default(),
    );
    let def = drivers::parse_spec(
        format_spec(addr, CheckKind::Format, &walk, ChecksOptions::default()),
        "v1",
    );
    let build = hbuiltins::pluginfs::file_addr("pkg/BUILD").format();
    let inputs: Vec<String> = def.inputs.iter().map(|i| i.r#ref.r#ref.format()).collect();
    assert_eq!(inputs, [build.clone()]);
    let outs: Vec<String> = def
        .outputs
        .iter()
        .flat_map(|o| &o.paths)
        .map(|p| {
            hbuiltins::pluginfs::output_fs_addr(&p.content)
                .expect("fs addr")
                .format()
        })
        .collect();
    assert_eq!(outs, [build]);

    // The helper's own type error is discarded; the bad call is pkg's.
    let found = lint::tests::lint_pkg(tmp.path(), "pkg", false);
    let at: Vec<(&str, usize)> = found.iter().map(|f| (f.file.as_str(), f.line)).collect();
    assert_eq!(at, [("pkg/BUILD", 7)], "{found:#?}");
    assert_eq!(found[0].kind, report::Kind::TypeError);
}

/// Each walked file becomes an fs dep `//@heph/fs:file@f=<path>`: the address
/// must parse back to the same path whatever the path holds.
#[cfg(test)]
#[test]
fn fs_addr_round_trip() {
    for path in [
        "pkg/BUILD",
        "with space/BUILD",
        "a=b/defs.star",
        "a@b/defs.star",
        "a:b/BUILD",
        "a,b/x.BUILD",
        "all =@:, of/them =@:,.star",
        "=lead/@x",
        "trail=/x,",
    ] {
        let addr = hbuiltins::pluginfs::file_addr(path);
        let text = addr.format();
        let parsed = hmodel::htaddr::parse_addr(&text)
            .unwrap_or_else(|e| panic!("{path:?}: {text} does not parse: {e:#}"));
        assert_eq!(parsed, addr, "{path:?}: {text}");
        assert_eq!(
            parsed.args.get("f").map(String::as_str),
            Some(path),
            "{text}"
        );
    }

    // And through the walk: every input's addr names it.
    let tmp = ws(&[
        ("a b/BUILD", "load(\"//c=d@e:f,g/defs.star\", \"X\")\n"),
        ("c=d@e:f,g/defs.star", "X = 1\n"),
    ]);
    let walk = walk(tmp.path(), "a b").expect("walk");
    let back: Vec<String> = walk
        .input_addrs()
        .map(|a| {
            hmodel::htaddr::parse_addr(&a.format())
                .expect("parses")
                .args["f"]
                .clone()
        })
        .collect();
    assert_eq!(back, ["a b/BUILD", "c=d@e:f,g/defs.star"]);
}

/// The failure text is the contract: one line per finding sorted by file,
/// line and column with its stable prefix and fix, the count, and the footer
/// for the kind of check — the `requireTypeAnnotations` line only when an
/// `annotation:` finding is present.
#[cfg(test)]
#[test]
fn failure_format() {
    use report::{ANNOTATIONS_FOOTER, Finding, Kind, format_failure, lint_failure};
    let at = |file: &str, line, col, kind, message: &str| Finding {
        file: file.to_string(),
        line,
        col,
        kind,
        message: message.to_string(),
    };
    let annotation = at(
        "pkg/BUILD",
        3,
        9,
        Kind::Annotation,
        "parameter `x` of `make_lib` has no type; add `: <type>`",
    );
    let type_error = at(
        "pkg/defs.BUILD",
        12,
        5,
        Kind::TypeError,
        "Expected type `str` but got `int`",
    );
    let earlier = at(
        "pkg/BUILD",
        3,
        2,
        Kind::TypeError,
        "Expected type `int` but got `str`",
    );

    // Given out of order: printed sorted by file, line, column.
    let failure = lint_failure(
        "pkg",
        &[type_error.clone(), annotation.clone(), earlier.clone()],
    )
    .expect("fails");
    // The count line is the run's error, on its own.
    assert_eq!(failure.summary, "3 findings in //pkg");
    assert_eq!(
        failure.log,
        format!(
            "pkg/BUILD:3:2: type-error: Expected type `int` but got `str`\n\
             pkg/BUILD:3:9: annotation: parameter `x` of `make_lib` has no type; add `: <type>`\n\
             pkg/defs.BUILD:12:5: type-error: Expected type `str` but got `int`\n\
             3 findings in //pkg\n\
             {ANNOTATIONS_FOOTER}"
        )
    );
    assert_eq!(
        ANNOTATIONS_FOOTER,
        "annotations are required by requireTypeAnnotations in .hephconfig (plugins: buildfile, options)"
    );
    // No annotation finding: no footer.
    let failure = lint_failure("pkg", &[type_error.clone()]).expect("fails");
    assert_eq!(
        failure.log,
        "pkg/defs.BUILD:12:5: type-error: Expected type `str` but got `int`\n1 finding in //pkg"
    );
    assert_eq!(failure.summary, "1 finding in //pkg");
    assert_eq!(lint_failure("pkg", &[]), None);

    let unformatted = at(
        "a/b/BUILD",
        4,
        1,
        Kind::Format,
        "not formatted; this is the first line that differs from the formatter's output",
    );
    let failure = format_failure("a/b", &[unformatted]).expect("fails");
    assert_eq!(
        failure.log,
        "a/b/BUILD:4:1: format: not formatted; this is the first line that differs from the \
         formatter's output\n1 finding in //a/b\nrun heph run //a/b:build-format to fix"
    );
    assert_eq!(failure.summary, "1 finding in //a/b");
    assert_eq!(format_failure("a/b", &[]), None);
}

/// I7: all three are cached locally, never remotely. Only `build-format` has
/// outputs, each an `in_place` file at the path of one of its fs inputs, so
/// the engine's in-place guard pairs them exactly; the two read-only checks
/// declare nothing.
#[cfg(test)]
#[test]
fn cache_and_codegen_flags() {
    use hplugin::driver::targetdef::path::CodegenMode;
    use targets::{CheckKind, ChecksOptions, format_spec, lint_spec};
    let tmp = ws(&[
        ("pkg/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
        ("pkg/more.BUILD", "Y = 1\n"),
        ("lib/defs.star", "X = 1\n"),
    ]);
    let walk = walk(tmp.path(), "pkg").expect("walk");
    for kind in CheckKind::ALL {
        let addr = hmodel::htaddr::Addr::new(
            hmodel::htpkg::PkgBuf::from("pkg"),
            kind.name().to_string(),
            Default::default(),
        );
        let opts = ChecksOptions::default();
        let spec = match kind {
            CheckKind::LintCheck => {
                lint_spec(addr, &walk, opts, &[], std::iter::empty(), "kwargs").expect("spec")
            }
            kind => format_spec(addr, kind, &walk, opts),
        };
        let def = drivers::parse_spec(spec, "v1");
        assert!(def.cache.enabled, "{kind:?}");
        assert!(!def.cache.remote_enabled, "{kind:?}");
        let inputs: Vec<String> = def.inputs.iter().map(|i| i.r#ref.r#ref.format()).collect();
        let reads: Vec<&str> = match kind {
            CheckKind::LintCheck => walk.inputs().collect(),
            _ => walk.owned().collect(),
        };
        let want: Vec<String> = reads
            .iter()
            .map(|p| hbuiltins::pluginfs::file_addr(p).format())
            .collect();
        assert_eq!(inputs, want, "{kind:?}");

        let paths: Vec<_> = def.outputs.iter().flat_map(|o| &o.paths).collect();
        if kind != CheckKind::Format {
            assert!(paths.is_empty(), "{kind:?}");
            continue;
        }
        let outs: Vec<String> = paths
            .iter()
            .map(|p| {
                assert!(matches!(p.codegen_tree, CodegenMode::InPlace { .. }));
                assert!(p.collect);
                hbuiltins::pluginfs::output_fs_addr(&p.content)
                    .expect("fs addr")
                    .format()
            })
            .collect();
        assert_eq!(outs, want);
    }
}
