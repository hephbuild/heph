//! The Go `format` target's shape, over bash: a `codegen = "in_place"` target
//! that reads its package's `*.go` through one fs glob and declares that same
//! glob as its output, next to a `codegen = "copy"` generator writing a `.go`
//! file into the same package.
//!
//! Go's `format` stopped listing its sources itself and declaring one
//! `FilePath` output per file; it reads them through `glob("<pkg>/*.go")` and
//! declares `Glob("<pkg>/*.go")`. These pin the engine's side of that: the
//! glob output is not read as colliding with the generator's `copy` output,
//! and the in-place guard pairs the glob input with the glob output.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use common::Workspace;
use heph::engine::Discovery;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use std::sync::Arc;

/// What the generator writes, and what must still be on disk after `fmt`.
const GENERATED: &str = "package p // generated";

/// A generator writing `p/gen.go` back into the tree, and a formatter that
/// uppercases every `.go` it was given, in place. `mutate` is bash spliced in
/// after the transform, to move the tree while `fmt` runs.
fn build_file(mutate: &str) -> String {
    format!(
        r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf '%s' '{GENERATED}' > $OUT",
    out = "gen.go",
    codegen = "copy",
)

target(
    name = "fmt",
    driver = "bash",
    deps = [glob("*.go")],
    out = "*.go",
    codegen = "in_place",
    run = "for f in *.go; do up=$(tr a-z A-Z < \"$f\"); printf '%s' \"$up\" > \"$f\"; done; {mutate}",
)
"#
    )
}

/// The `format` gate before merge: the formatter's `p/*.go` glob output and
/// the generator's `p/gen.go` copy output live in one package, and neither the
/// overlap check `heph validate` runs nor the run itself treats them as two
/// writers of one file. The generated file is not the formatter's input, so it
/// is neither collected nor written back.
#[tokio::test]
async fn format_glob_output_does_not_overlap_copy_outputs() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", &build_file("true"));
    ws.write_file("p/src.go", "package p");

    let overlaps = Arc::clone(&ws.engine)
        .codegen_copy_overlaps(
            ws.engine.new_state(),
            &Matcher::TreeOutputTo(PkgBuf::from("")),
            Discovery::Complete,
        )
        .await?;
    assert!(
        overlaps.is_empty(),
        "an in_place glob next to a copy output is not a collision: {overlaps:?}"
    );

    ws.run("//p:gen").await?;
    let fmt = ws.run("//p:fmt").await?;

    let collected: Vec<String> = fmt
        .artifacts
        .iter()
        .flat_map(|a| {
            a.walk()
                .expect("walk fmt artifact")
                .map(|e| e.expect("fmt artifact entry").path.display().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        collected,
        vec!["p/src.go".to_string()],
        "the glob output collects the sources it staged, never the generated file"
    );

    let root = ws.dir.path();
    assert_eq!(std::fs::read_to_string(root.join("p/src.go"))?, "PACKAGE P");
    assert_eq!(
        std::fs::read_to_string(root.join("p/gen.go"))?,
        GENERATED,
        "the generator's file is not the formatter's to rewrite"
    );
    Ok(())
}

/// The in-place guard pairs the `p/*.go` glob input with the `p/*.go` glob
/// output, with a generator's file sitting in the package: a settled tree
/// writes back and re-runs clean (no false positive from the generated file),
/// and a source edited while `fmt` runs is caught and kept — which only
/// happens if the guard found the glob input to re-read.
#[tokio::test]
async fn in_place_inputs_unchanged_with_glob_output() -> anyhow::Result<()> {
    let settled = Workspace::new();
    settled.write_build_file("p", &build_file("true"));
    settled.write_file("p/src.go", "package p");
    settled.run("//p:gen").await?;

    settled.run("//p:fmt").await?;
    let src = settled.dir.path().join("p/src.go");
    assert_eq!(std::fs::read_to_string(&src)?, "PACKAGE P");
    settled.run("//p:fmt").await?;
    assert_eq!(
        std::fs::read_to_string(&src)?,
        "PACKAGE P",
        "a re-run over the already-formatted tree must change nothing"
    );

    let racing = Workspace::new();
    let src = racing.dir.path().join("p/src.go");
    // Absolute path: this write deliberately escapes the sandbox to model an
    // external edit landing mid-run, which is what the guard watches for.
    racing.write_build_file(
        "p",
        &build_file(&format!(
            "printf 'edited by someone else' > {}",
            src.display()
        )),
    );
    racing.write_file("p/src.go", "package p");
    racing.run("//p:gen").await?;

    let msg = match racing.run("//p:fmt").await {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("a stale in-place write-back over a glob output must fail, not clobber"),
    };
    assert!(
        msg.contains("changed while it ran"),
        "error must name the staleness, got: {msg}"
    );
    assert_eq!(
        std::fs::read_to_string(&src)?,
        "edited by someone else",
        "the newer bytes must survive: nothing is written back"
    );
    assert_eq!(
        std::fs::read_to_string(racing.dir.path().join("p/gen.go"))?,
        GENERATED
    );
    Ok(())
}
