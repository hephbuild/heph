//! The BUILD-file checks: `build-lint-check`, `build-format-check` and
//! `build-format` (docs/BUILDFILE_CHECKS.md).
//!
//! - [`loads`]: the parse-only load walk (I8) and the ownership rule (I1).

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the walk is consumed by the check targets, which land in a later layer of this stack"
    )
)]

pub(crate) mod loads;

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
