//! `codegen = "in_place"` write-back: a target that rewrites the very files it
//! took as inputs (a formatter, a lint fixer).

mod common;

use common::Workspace;

/// A target that uppercases its own source in place.
///
/// `mutate` is bash spliced in after the transform: the test uses it to move the
/// tree *while the target runs*, standing in for the editor save / `git checkout`
/// / concurrent run that the write-back guard exists to catch.
fn build_file(mutate: &str) -> String {
    format!(
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    codegen = "in_place",
    run = "up=$(tr a-z A-Z < src.txt); printf '%s' \"$up\" > $OUT; {mutate}",
)
"#
    )
}

/// The happy path: a settled tree is transformed and written back, and running
/// again over the already-transformed tree is a clean no-op. The second run is
/// what proves the guard doesn't false-positive on the write-back's *own*
/// change to the tree.
#[tokio::test]
async fn in_place_writes_back_and_reruns_clean() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &build_file("true"));
    ws.write_file("fix/src.txt", "hello");

    ws.run("//fix:upper").await?;
    let src = ws.dir.path().join("fix/src.txt");
    assert_eq!(
        std::fs::read_to_string(&src)?,
        "HELLO",
        "the transformed bytes must land on the tracked source file"
    );

    ws.run("//fix:upper").await?;
    assert_eq!(
        std::fs::read_to_string(&src)?,
        "HELLO",
        "a re-run over the already-transformed tree must change nothing"
    );
    Ok(())
}

/// The guard: when the source moves between being hashed and being written back,
/// the transform is stale — it was computed from the older bytes. Writing it
/// would silently discard the newer ones, so the run fails and the tree is left
/// exactly as the concurrent writer left it.
#[tokio::test]
async fn in_place_refuses_to_write_back_over_a_changed_source() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let src = ws.dir.path().join("fix/src.txt");
    // Absolute path: this write deliberately escapes the sandbox to model an
    // *external* edit landing mid-run, which is exactly what the guard watches
    // for. No ordinary target may do this.
    ws.write_build_file(
        "fix",
        &build_file(&format!(
            "printf 'edited by someone else' > {}",
            src.display()
        )),
    );
    ws.write_file("fix/src.txt", "hello");

    let msg = match ws.run("//fix:upper").await {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("a stale in-place write-back must fail, not clobber"),
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
    Ok(())
}
