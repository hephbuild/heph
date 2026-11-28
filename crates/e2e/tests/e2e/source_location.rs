//! Where a failure about a target's declaration points (error-reporting F3,
//! D3): the option the error is about, found by re-parsing the declaring
//! file on the error path, else the declaring call.
//!
//! Each test reads the recorded failure's `diagnostic.location`. The
//! provider half (finding an option inside a call) is unit-tested next to
//! the buildfile provider's `source_index`; these pin what the engine asks
//! for, and that a report parses each file once.
#![expect(
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use hcore::diagnostic::SourceLoc;
use heph::engine::request_state::RequestState;
use heph::engine::{OutputMatcher, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::Matcher;
use hplugin::error::TargetFailure;
use std::sync::Arc;

fn addr(a: &str) -> Addr {
    parse_addr(a).unwrap_or_else(|e| panic!("parse {a}: {e:#}"))
}

/// A bash target writing `$OUT`, in BUILD-file syntax, with `extra` options.
fn t(name: &str, extra: &str) -> String {
    format!(
        r#"target(name = "{name}", driver = "bash", run = "echo ok > $OUT", out = "o.txt", {extra})"#
    )
}

/// `d/BUILD` evaluates and defines only `present`.
const D_PRESENT: &str =
    r#"target(name = "present", driver = "bash", run = "echo ok > $OUT", out = "o.txt")"#;

/// `targets` in request `rs`, which must fail; the failures it recorded.
async fn fail_in(
    ws: &Workspace,
    rs: &Arc<RequestState>,
    targets: &[&str],
) -> Vec<Arc<TargetFailure>> {
    let m = Matcher::Or(targets.iter().map(|a| Matcher::Addr(addr(a))).collect());
    let res = Arc::clone(&ws.engine)
        .result(
            rs.clone(),
            &m,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let failed = match res {
        Ok(batch) => !batch.errors.is_empty(),
        Err(_) => true,
    };
    assert!(failed, "{targets:?} must fail");
    rs.take_failures()
}

/// `a` in a fresh request: its one recorded failure, which must blame `a`.
async fn fail(ws: &Workspace, a: &str) -> Arc<TargetFailure> {
    let rs = ws.engine.new_state_with_events(false, None);
    one(fail_in(ws, &rs, &[a]).await, a)
}

fn one(failures: Vec<Arc<TargetFailure>>, a: &str) -> Arc<TargetFailure> {
    match failures.as_slice() {
        [f] => {
            assert_eq!(f.culprit().to_string(), a, "{}", text(f));
            Arc::clone(f)
        }
        fs => panic!("one failure of {a}, got {}", fs.len()),
    }
}

fn text(f: &TargetFailure) -> String {
    crate::common::failure_text(f)
}

fn loc(f: &TargetFailure) -> &SourceLoc {
    f.record
        .diagnostic
        .location
        .as_ref()
        .unwrap_or_else(|| panic!("a location on: {}", text(f)))
}

/// `(file, line, col)` of the first `needle` in `src`, 1-based: where a
/// diagnostic about the option spelled `needle` points.
fn at(file: &str, src: &str, needle: &str) -> (String, u32, u32) {
    let (i, line) = src
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} in {src:?}"));
    let col = line.find(needle).unwrap_or_default();
    (
        file.to_owned(),
        u32::try_from(i + 1).unwrap_or_default(),
        u32::try_from(col + 1).unwrap_or_default(),
    )
}

fn triple(l: &SourceLoc) -> (String, u32, u32) {
    (l.file.clone(), l.line, l.col)
}

/// A one-target `c/BUILD` (a comment line first, so the call is on line 2)
/// whose target `//c:y` carries `extra`; and `d/BUILD`.
fn ws_with(extra: &str) -> (Workspace, String) {
    let src = format!("# y\n{}\n", t("y", extra));
    let ws = Workspace::new();
    ws.write_build_file("c", &src);
    ws.write_build_file("d", D_PRESENT);
    (ws, src)
}

#[tokio::test]
async fn unknown_option_carries_kwarg_location() {
    let (ws, src) = ws_with("bogus = 1");
    let f = fail(&ws, "//c:y").await;
    assert_eq!(
        triple(loc(&f)),
        at("c/BUILD", &src, "bogus"),
        "{}",
        text(&f)
    );
}

#[tokio::test]
async fn wrong_type_carries_kwarg_location() {
    let src = "# y\ntarget(name = \"y\", driver = \"bash\", out = \"o.txt\", run = 3)\n";
    let ws = Workspace::new();
    ws.write_build_file("c", src);
    let f = fail(&ws, "//c:y").await;
    assert_eq!(triple(loc(&f)), at("c/BUILD", src, "run ="), "{}", text(&f));
}

#[tokio::test]
async fn missing_dep_carries_kwarg_location() {
    let (ws, src) = ws_with(r#"deps = ["//d:missing"]"#);
    let f = fail(&ws, "//c:y").await;
    assert_eq!(triple(loc(&f)), at("c/BUILD", &src, "deps"), "{}", text(&f));
}

#[tokio::test]
async fn bad_deps_string_carries_location() {
    let (ws, src) = ws_with(r#"deps = ["::not an addr"]"#);
    let f = fail(&ws, "//c:y").await;
    assert_eq!(triple(loc(&f)), at("c/BUILD", &src, "deps"), "{}", text(&f));
}

#[tokio::test]
async fn unknown_driver_carries_location() {
    let src = "# y\ntarget(name = \"y\", driver = \"bsh\", run = \"x\")\n";
    let ws = Workspace::new();
    ws.write_build_file("c", src);
    let f = fail(&ws, "//c:y").await;
    assert!(text(&f).contains("driver not found: bsh"), "{}", text(&f));
    assert_eq!(
        triple(loc(&f)),
        at("c/BUILD", src, "driver"),
        "{}",
        text(&f)
    );
}

/// A target a macro declares is located in the `.bzl`, at the macro's own
/// `target()` call: the option is the `.bzl`'s, not the BUILD file's call
/// to the macro. Its `name=` is computed, so no name check applies.
#[tokio::test]
async fn macro_declared_target_names_bzl_frame() {
    let bzl = "def mk(name):\n    target(\n        name = name,\n        driver = \"bash\",\n        run = \"echo ok > $OUT\",\n        out = \"o.txt\",\n        deps = [\"//d:missing\"],\n    )\n";
    let ws = Workspace::new();
    ws.write_file("tools/macros.bzl", bzl);
    ws.write_build_file(
        "c",
        "load(\"//tools/macros.bzl\", \"mk\")\n\nmk(name = \"y\")\n",
    );
    ws.write_build_file("d", D_PRESENT);
    let f = fail(&ws, "//c:y").await;
    assert_eq!(
        triple(loc(&f)),
        at("tools/macros.bzl", bzl, "deps"),
        "{}",
        text(&f)
    );
}

/// `//c:y`'s spec resolved in `rs`, then `c/BUILD` rewritten to `edited`
/// before the failure asks where it was declared.
async fn fail_after_edit(src: &str, edited: &str) -> Arc<TargetFailure> {
    let ws = Workspace::new();
    ws.write_build_file("c", src);
    ws.write_build_file("d", D_PRESENT);
    let rs = ws.engine.new_state_with_events(false, None);
    Arc::clone(&ws.engine)
        .get_spec(rs.clone(), &addr("//c:y"))
        .await
        .expect("//c:y resolves");
    ws.write_build_file("c", edited);
    one(fail_in(&ws, &rs, &["//c:y"]).await, "//c:y")
}

/// `f` points at `//c:y`'s call as evaluated, line 2 of `c/BUILD`, and says
/// the file changed since, in the message the record and both renderers
/// carry.
fn at_call_changed(f: &Arc<TargetFailure>) {
    assert_eq!(triple(loc(f)), ("c/BUILD".to_owned(), 2, 1), "{}", text(f));
    assert!(
        f.record
            .diagnostic
            .message
            .contains("file changed since evaluation"),
        "{}",
        f.record.diagnostic.message
    );
}

/// The file moved under the call since it evaluated: no call starts where
/// the spec says, so the diagnostic points at the call as evaluated.
#[tokio::test]
async fn edited_file_falls_back_to_call_line() {
    let src = format!("# y\n{}\n", t("y", r#"deps = ["//d:missing"]"#));
    at_call_changed(&fail_after_edit(&src, &format!("\n\n\n{src}")).await);
}

/// The call at the spec's span now declares another name: its options are
/// not this target's, so the diagnostic points at the call.
#[tokio::test]
async fn call_name_mismatch_falls_back_to_call_line() {
    let src = format!("# y\n{}\n", t("y", r#"deps = ["//d:missing"]"#));
    at_call_changed(&fail_after_edit(&src, &src.replace("name = \"y\"", "name = \"z\"")).await);
}

/// The file no longer parses since it evaluated: nothing in it can be
/// located, so the diagnostic points at the call as evaluated. A failure
/// the engine locates (an unknown option), not one a `MissingDep` carries.
#[tokio::test]
async fn unparsable_file_falls_back_to_call_line() {
    let src = format!("# y\n{}\n", t("y", "bogus = 1"));
    at_call_changed(&fail_after_edit(&src, "target(").await);
}

/// The file was deleted since it evaluated: the `.bzl` whose macro declared
/// the target (deleting the BUILD file would drop the package from the
/// match instead).
#[tokio::test]
async fn deleted_file_falls_back_to_call_line() {
    let bzl = "# m\ndef mk(name):\n    target(name = name, driver = \"bash\", run = \"echo ok > $OUT\", out = \"o.txt\", deps = [\"//d:missing\"])\n";
    let ws = Workspace::new();
    ws.write_file("tools/macros.bzl", bzl);
    ws.write_build_file(
        "c",
        "load(\"//tools/macros.bzl\", \"mk\")\n\nmk(name = \"y\")\n",
    );
    ws.write_build_file("d", D_PRESENT);
    let rs = ws.engine.new_state_with_events(false, None);
    Arc::clone(&ws.engine)
        .get_spec(rs.clone(), &addr("//c:y"))
        .await
        .expect("//c:y resolves");
    std::fs::remove_file(ws.dir.path().join("tools/macros.bzl")).expect("delete the .bzl");
    let f = one(fail_in(&ws, &rs, &["//c:y"]).await, "//c:y");
    assert_eq!(
        triple(loc(&f)),
        ("tools/macros.bzl".to_owned(), 3, 5),
        "{}",
        text(&f)
    );
    assert!(
        text(&f).contains("file changed since evaluation"),
        "{}",
        text(&f)
    );
}

/// An unchanged file says nothing of a change.
#[tokio::test]
async fn an_unchanged_file_is_not_called_changed() {
    let (ws, _) = ws_with("bogus = 1");
    let f = fail(&ws, "//c:y").await;
    assert!(
        !text(&f).contains("file changed since evaluation"),
        "{}",
        text(&f)
    );
}

/// A target a function declares (a matrix cell), in an unchanged file, is
/// located at its option and is not called changed: the declaring call the
/// index finds is the cell's own `target()`, not the function's call site.
#[tokio::test]
async fn a_matrix_cell_in_an_unchanged_file_is_not_called_changed() {
    let src = concat!(
        "def _cell(v):\n",
        "    target(name = \"y\", driver = \"bash\", run = \"echo ok > $OUT\", out = \"o.txt\", bogus = 1)\n",
        "    return \":y\"\n",
        "\n",
        "M = heph.core.matrix(axes = {\"v\": [\"one\"]}, cell = _cell)\n",
    );
    let ws = Workspace::new();
    ws.write_build_file("c", src);
    let f = fail(&ws, "//c:y").await;
    assert_eq!(triple(loc(&f)), at("c/BUILD", src, "bogus"), "{}", text(&f));
    assert!(
        !text(&f).contains("file changed since evaluation"),
        "{}",
        text(&f)
    );
}

/// Three failures in one file, each located at its own option: the
/// provider is asked for the file's index once.
#[tokio::test]
async fn one_reparse_per_file_per_report() {
    let src = format!(
        "{}\n{}\n{}\n",
        t("a", r#"deps = ["//d:missing"]"#),
        t("b", "bogus = 1"),
        t("c", r#"tools = ["//d:missing"]"#),
    );
    let ws = Workspace::new();
    ws.write_build_file("c", &src);
    ws.write_build_file("d", D_PRESENT);
    let rs = ws.engine.new_state_with_events(false, None);
    let mut failures = fail_in(&ws, &rs, &["//c:a", "//c:b", "//c:c"]).await;
    failures.sort_by_key(|f| f.culprit().to_string());
    let got: Vec<_> = failures.iter().map(|f| triple(loc(f))).collect();
    let lines: Vec<&str> = src.lines().collect();
    let want: Vec<_> = [("deps", 0), ("bogus", 1), ("tools", 2)]
        .iter()
        .map(|(needle, i)| {
            let (file, _, col) = at("c/BUILD", lines[*i], needle);
            (file, u32::try_from(i + 1).unwrap_or_default(), col)
        })
        .collect();
    assert_eq!(got, want);
    assert_eq!(rs.source_index_loads(), 1, "one parse of c/BUILD");
}
