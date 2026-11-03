//! A `glob()` dep's cache key must cover *which path* holds *which bytes*.
//!
//! A consumer's `hashin` folds its def hash and the sorted hashouts of its
//! input artifacts — no artifact names, no paths. A glob addr carries only the
//! pattern, so each glob entry's hashout is the only place a matched file's
//! path can reach the key. When it was a pure content hash, renaming a file
//! (keeping its bytes) or swapping two files' contents left the sorted
//! multiset unchanged, and the consumer served a result computed for a tree
//! that no longer exists.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use common::Workspace;
use heph::engine::{OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;

/// A target that prints every globbed file's name next to its bytes, so its
/// output changes exactly when the (path → bytes) mapping does.
const LISTING_BUILD: &str = r#"
target(
    name = "t",
    driver = "bash",
    deps = {"src": [glob("src/*.txt")]},
    out = "out.txt",
    run = 'for f in $SRC_SRC; do echo "$(basename $f)=$(cat $f)"; done | sort > $OUT',
)
"#;

/// Run `//pkg:t` the way the *next* `heph` invocation would: a fresh engine
/// over the same on-disk cache, so nothing in-memory carries over.
async fn run_fresh(ws: &Workspace) -> anyhow::Result<String> {
    let engine = ws.reopen()?;
    let rs = engine.new_state();
    let addr = parse_addr("//pkg:t")?;
    let res = engine
        .clone()
        .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
        .await?;
    Ok(common::artifact_string(&res))
}

/// Renaming a globbed file while keeping its bytes must re-run the consumer.
/// `a_linux.go` → `a_darwin.go` under `*.go` is the real-world shape: same
/// bytes, different meaning.
#[tokio::test]
async fn renaming_a_globbed_file_reruns_the_consumer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("pkg/src/a.txt", "one");
    ws.write_file("pkg/src/b.txt", "two");
    ws.write_build_file("pkg", LISTING_BUILD);

    let first = run_fresh(&ws).await?;
    assert_eq!(first, "a.txt=one\nb.txt=two\n");

    let src = common::root(&ws).join("pkg/src");
    std::fs::rename(src.join("a.txt"), src.join("c.txt"))?;

    let second = run_fresh(&ws).await?;
    assert_eq!(
        second, "b.txt=two\nc.txt=one\n",
        "a renamed glob entry must move the consumer's cache key"
    );
    Ok(())
}

/// Swapping the contents of two globbed files leaves the multiset of content
/// hashes unchanged; the consumer must still re-run.
#[tokio::test]
async fn swapping_two_globbed_files_contents_reruns_the_consumer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    // Different lengths, so the walker's stat-based revalidation sees each
    // rewrite even within one mtime tick.
    ws.write_file("pkg/src/a.txt", "one");
    ws.write_file("pkg/src/b.txt", "three");
    ws.write_build_file("pkg", LISTING_BUILD);

    let first = run_fresh(&ws).await?;
    assert_eq!(first, "a.txt=one\nb.txt=three\n");

    ws.write_file("pkg/src/a.txt", "three");
    ws.write_file("pkg/src/b.txt", "one");

    let second = run_fresh(&ws).await?;
    assert_eq!(
        second, "a.txt=three\nb.txt=one\n",
        "swapped glob entry contents must move the consumer's cache key"
    );
    Ok(())
}
