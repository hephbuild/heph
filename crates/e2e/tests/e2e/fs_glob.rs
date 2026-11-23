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
use hbuiltins::pluginfs::holds_request;
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

/// The go provider's source glob, through the real engine: rooted at the
/// package (`<pkg>/**/*`) with its `.go`/module-file excludes. The fs driver
/// compiles only the part past the literal prefix and matches it against the
/// path with the prefix stripped, so this pins that the split is invisible: a
/// near-miss sibling (`pkgx`), a same-named directory deeper in the tree
/// (`other/pkg`) and excluded files stay out, and nested files stay in.
#[tokio::test]
async fn package_rooted_glob_sources_exactly_its_own_tree() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("pkg/a.txt", "a");
    ws.write_file("pkg/sub/deep/b.txt", "b");
    ws.write_file("pkg/inner/e.txt", "e");
    ws.write_file("pkg/x.go", "package x");
    ws.write_file("pkg/go.mod", "module x");
    ws.write_file("pkgx/c.txt", "c");
    ws.write_file("other/pkg/d.txt", "d");
    ws.write_build_file(
        "pkg",
        r#"
target(
    name = "t",
    driver = "bash",
    deps = {"src": [glob("**/*", exclude = ["**/*.go", "**/go.mod", "**/go.sum", "BUILD"])]},
    out = "out.txt",
    run = 'for f in $SRC_SRC; do echo "${f##*/pkg/}"; done | sort > $OUT',
)
"#,
    );

    let res = ws.run("//pkg:t").await?;
    assert_eq!(
        common::artifact_string(&res),
        "a.txt\ninner/e.txt\nsub/deep/b.txt\n",
        "a package-rooted glob must source exactly its own tree, minus its excludes"
    );
    Ok(())
}

/// Waits up to 10s for `cond`: a request's data drops once the last task
/// riding it lets go, which can trail the `drop` of its root handles.
async fn eventually(cond: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !cond() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    true
}

/// The fs plugin memoizes a request's glob walks and compiled excludes under
/// the request's id, in process-wide caches. Ending a request must drop its
/// buckets — the engine reaches `pluginfs::forget_request` with the id the
/// driver keyed them by — and only its own: a request still alive on the same
/// engine keeps its memoized walk.
#[tokio::test]
async fn an_ended_request_drops_its_glob_memos_and_a_live_one_keeps_them() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("pkg/src/a.txt", "one");
    ws.write_file("pkg/src/b.tmp", "skipped");
    ws.write_build_file(
        "pkg",
        r#"
target(
    name = "t",
    driver = "bash",
    deps = {"src": [glob("src/*", exclude = ["src/*.tmp"])]},
    out = "out.txt",
    run = 'for f in $SRC_SRC; do basename $f; done | sort > $OUT',
)
"#,
    );
    let engine = ws.reopen()?;
    let addr = parse_addr("//pkg:t")?;

    let ended = engine.new_state();
    let ended_id = ended.request_id().clone();
    let res = engine
        .clone()
        .result_addr(
            ended.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    assert_eq!(common::artifact_string(&res), "a.txt\n");
    assert!(
        holds_request(&ended_id),
        "the glob run memoized its walk under the request's id"
    );

    let live = engine.new_state();
    let live_id = live.request_id().clone();
    let live_res = engine
        .clone()
        .result_addr(
            live.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    assert_eq!(common::artifact_string(&live_res), "a.txt\n");
    assert!(holds_request(&live_id));

    drop(res);
    drop(ended);
    assert!(
        eventually(|| !holds_request(&ended_id)).await,
        "an ended request's glob memos must be dropped"
    );
    assert!(
        holds_request(&live_id),
        "a live request keeps its memoized walk when another request ends"
    );

    drop(live_res);
    drop(live);
    assert!(
        eventually(|| !holds_request(&live_id)).await,
        "the second request's memos go when it ends too"
    );
    Ok(())
}
