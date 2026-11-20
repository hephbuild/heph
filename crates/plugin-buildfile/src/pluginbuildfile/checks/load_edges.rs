//! `checks::load_edges`: one test per row of the spec's "Adversarial inputs,
//! load walk" table. Each failing row also holds the walk to evaluation's
//! message ([`assert_matches_eval`]).

use super::loads::LoadEdge;
use super::test_ws::{assert_matches_eval, walk, ws};

fn failure(files: &[(&str, &str)], pkg: &str, name: &str) -> String {
    let tmp = ws(files);
    assert_matches_eval(tmp.path(), pkg, name)
        .unwrap_or_else(|| panic!("{name}: expected the walk to fail"))
}

#[test]
fn cycle_between_two_files() {
    let msg = failure(
        &[
            ("a/BUILD", "load(\"./x.star\", \"X\")\n"),
            ("a/x.star", "load(\"./y.star\", \"Y\")\nX = 1\n"),
            ("a/y.star", "load(\"./x.star\", \"X\")\nY = 1\n"),
        ],
        "a",
        "A -> B -> A",
    );
    assert_eq!(msg, "load() cycle detected at a/x.star");
}

#[test]
fn self_load_is_a_cycle() {
    let msg = failure(
        &[("a/BUILD", "load(\"./BUILD\", \"X\")\nX = 1\n")],
        "a",
        "self-load",
    );
    assert_eq!(msg, "load() cycle detected at a/BUILD");
}

#[test]
fn cross_package_cycle() {
    let msg = failure(
        &[
            ("a/BUILD", "load(\"//b\", \"Y\")\nX = 1\n"),
            ("b/BUILD", "load(\"//a\", \"X\")\nY = 1\n"),
        ],
        "a",
        "cross-package cycle",
    );
    assert_eq!(msg, "load() cycle detected at a");
}

#[test]
fn dir_load_records_every_pattern_file() {
    let tmp = ws(&[
        ("a/BUILD", "load(\"//lib\", \"X\")\n"),
        ("lib/BUILD", "X = 1\n"),
        ("lib/more.BUILD", "Y = 2\n"),
        ("lib/notes.txt", "not starlark ((\n"),
    ]);
    let walk = walk(tmp.path(), "a").expect("walk");
    assert_eq!(
        walk.files["a/BUILD"].loads["//lib"],
        LoadEdge::Dir {
            dir: "lib".into(),
            pkg: "lib".into(),
            files: vec!["lib/BUILD".into(), "lib/more.BUILD".into()],
        }
    );
    assert!(walk.files.contains_key("lib/more.BUILD"));
    assert!(!walk.files.contains_key("lib/notes.txt"));
    assert_matches_eval(tmp.path(), "a", "dir load");
}

#[test]
fn escaping_the_root_is_the_resolvers_error() {
    let msg = failure(
        &[("a/BUILD", "load(\"../../../etc/hosts\", \"X\")\n")],
        "a",
        "escape",
    );
    assert!(
        msg.starts_with("resolving load() path `../../../etc/hosts`"),
        "{msg}"
    );
}

#[test]
fn bare_path_is_the_resolvers_error() {
    let msg = failure(
        &[("a/BUILD", "load(\"defs.star\", \"X\")\n")],
        "a",
        "bare path",
    );
    assert_eq!(
        msg,
        "load() path must start with `//`, `./`, or `../`, got `defs.star`"
    );
}

#[test]
fn slashes_alone_is_the_resolvers_error() {
    let msg = failure(&[("a/BUILD", "load(\"//\", \"X\")\n")], "a", "//");
    assert_eq!(msg, "load() path must not be empty after `//`");
}

#[test]
fn trailing_slash_is_trimmed() {
    let tmp = ws(&[
        ("a/BUILD", "load(\"//lib/defs.star/\", \"X\")\n"),
        ("lib/defs.star", "X = 1\n"),
    ]);
    let walk = walk(tmp.path(), "a").expect("walk");
    assert_eq!(
        walk.files["a/BUILD"].loads["//lib/defs.star/"],
        LoadEdge::File {
            path: "lib/defs.star".into(),
            pkg: "lib".into()
        }
    );
    assert_matches_eval(tmp.path(), "a", "trailing slash");
}

#[test]
fn missing_file_is_evaluations_error() {
    let msg = failure(
        &[("a/BUILD", "load(\"//lib/nope.star\", \"X\")\n")],
        "a",
        "missing file",
    );
    assert!(msg.starts_with("stat load path lib/nope.star"), "{msg}");
}

#[test]
fn symlinked_file_resolves_as_evaluation_does() {
    let tmp = ws(&[
        ("a/BUILD", "load(\"./link.star\", \"X\")\n"),
        ("lib/real.star", "X = 1\n"),
    ]);
    std::os::unix::fs::symlink("../lib/real.star", tmp.path().join("a/link.star"))
        .expect("symlink");
    let walk = walk(tmp.path(), "a").expect("walk");
    assert_eq!(
        walk.files["a/BUILD"].loads["./link.star"],
        LoadEdge::File {
            path: "a/link.star".into(),
            pkg: "a".into()
        }
    );
    assert!(!walk.files.contains_key("lib/real.star"));
    assert_matches_eval(tmp.path(), "a", "symlinked file");
}

#[test]
fn diamond_records_the_shared_file_once() {
    let tmp = ws(&[
        (
            "a/BUILD",
            "load(\"//b/b.star\", \"B\")\nload(\"//c/c.star\", \"C\")\n",
        ),
        ("b/b.star", "load(\"//d/d.star\", \"D\")\nB = D\n"),
        ("c/c.star", "load(\"../d/d.star\", \"D\")\nC = D\n"),
        ("d/d.star", "D = 1\n"),
    ]);
    let walk = walk(tmp.path(), "a").expect("walk");
    assert_eq!(
        walk.inputs().collect::<Vec<_>>(),
        ["a/BUILD", "b/b.star", "c/c.star", "d/d.star"]
    );
    let d = LoadEdge::File {
        path: "d/d.star".into(),
        pkg: "d".into(),
    };
    assert_eq!(walk.files["b/b.star"].loads["//d/d.star"], d);
    assert_eq!(walk.files["c/c.star"].loads["../d/d.star"], d);
    let order = walk.dependency_order().expect("order");
    let pos = |f: &str| order.iter().position(|o| *o == f).expect("in order");
    assert!(pos("d/d.star") < pos("b/b.star") && pos("d/d.star") < pos("c/c.star"));
    assert!(pos("b/b.star") < pos("a/BUILD") && pos("c/c.star") < pos("a/BUILD"));
    assert_matches_eval(tmp.path(), "a", "diamond");
}

#[test]
fn depth_two_is_in_the_set() {
    let tmp = ws(&[
        ("a/BUILD", "load(\"//b/b.star\", \"B\")\n"),
        ("b/b.star", "load(\"//c/c.star\", \"C\")\nB = C\n"),
        ("c/c.star", "C = 1\n"),
    ]);
    let walk = walk(tmp.path(), "a").expect("walk");
    assert!(walk.inputs().any(|f| f == "c/c.star"));
    assert_eq!(
        walk.dependency_order().expect("order"),
        ["c/c.star", "b/b.star", "a/BUILD"]
    );
    assert_matches_eval(tmp.path(), "a", "depth 2");
}

#[test]
fn sibling_helper_is_owned_by_its_directory() {
    let tmp = ws(&[
        ("x/BUILD", "X = 1\n"),
        ("x/defs.BUILD", "D = 1\n"),
        ("y/BUILD", "load(\"//x/defs.BUILD\", \"D\")\n"),
    ]);
    let y = walk(tmp.path(), "y").expect("walk y");
    assert!(y.inputs().any(|f| f == "x/defs.BUILD"));
    assert!(!y.owned().any(|f| f == "x/defs.BUILD"));
    let x = walk(tmp.path(), "x").expect("walk x");
    assert!(x.owned().any(|f| f == "x/defs.BUILD"));
    assert_matches_eval(tmp.path(), "y", "sibling helper");
}
