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

/// A names-only glob (`m=names`, reachable from Rust as
/// `pluginfs::names_glob_addr`) through the real engine and sandbox: each
/// entry is staged as an empty, non-executable placeholder at its path; editing
/// a file's bytes leaves the consumer cached, while renaming one re-runs it.
#[tokio::test]
async fn names_glob_stages_placeholders_and_keys_on_names_only() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("pkg/src/a.txt", "one");
    ws.write_file("pkg/src/b.txt", "three");
    ws.write_build_file(
        "pkg",
        r#"
target(
    name = "t",
    driver = "bash",
    deps = {"src": ["//@heph/fs:glob@p=pkg/src/*,m=names"]},
    out = "out.txt",
    run = 'for f in $SRC_SRC; do echo "$(basename $f) size=$(wc -c < $f | tr -d " ") x=$(test -x $f && echo y || echo n) $RANDOM"; done | sort > $OUT',
)
"#,
    );
    let strip_random = |s: &str| -> String {
        s.lines()
            .map(|l| l.rsplit_once(' ').map_or(l, |(head, _)| head))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let first = run_fresh(&ws).await?;
    assert_eq!(
        strip_random(&first),
        "a.txt size=0 x=n\nb.txt size=0 x=n",
        "names entries are staged as empty, non-executable placeholders"
    );

    ws.write_file("pkg/src/a.txt", "a much longer body");
    let edited = run_fresh(&ws).await?;
    assert_eq!(
        edited, first,
        "editing a file's bytes must not move a names glob consumer's key"
    );

    let src = common::root(&ws).join("pkg/src");
    std::fs::rename(src.join("a.txt"), src.join("c.txt"))?;
    let renamed = run_fresh(&ws).await?;
    assert_eq!(strip_random(&renamed), "b.txt size=0 x=n\nc.txt size=0 x=n");
    Ok(())
}

/// [`run_fresh`], also returning every build event the request emitted.
async fn run_fresh_events(
    ws: &Workspace,
) -> anyhow::Result<(String, Vec<heph::engine::event::BuildEvent>)> {
    let engine = ws.reopen()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let addr = parse_addr("//pkg:t")?;
    let res = engine
        .clone()
        .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
        .await?;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    Ok((common::artifact_string(&res), events))
}

/// A consumer that lists each staged entry's path relative to `pkg/src`.
const NAMES_PATHS_BUILD: &str = r#"
target(
    name = "t",
    driver = "bash",
    deps = {"src": ["//@heph/fs:glob@p=pkg/src/**/*,m=names"]},
    out = "out.txt",
    run = 'for f in $SRC_SRC; do echo "${f#*pkg/src/}"; done | sort > $OUT',
)
"#;

/// A names glob's placeholders are owned, in-memory bytes on the uncached path:
/// they are handed to the consumer as they are, never packed into the tmp
/// store. Packing them cost a tar job and a store insert per entry on every
/// run, a consumer's cache hit included, since the glob itself always re-runs.
#[tokio::test]
async fn names_glob_never_writes_the_tmp_store() -> anyhow::Result<()> {
    use heph::engine::event::BuildEventKind;

    let ws = Workspace::new();
    ws.write_file("pkg/src/a.txt", "one");
    ws.write_file("pkg/src/b.txt", "two");
    ws.write_build_file("pkg", NAMES_PATHS_BUILD);

    let glob_writes = |events: &[heph::engine::event::BuildEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                BuildEventKind::LocalCacheWriteStart { addr } if addr.contains("@heph/fs") => {
                    Some(addr.clone())
                }
                _ => None,
            })
            .collect()
    };

    let (cold, cold_events) = run_fresh_events(&ws).await?;
    assert_eq!(cold, "a.txt\nb.txt\n");
    assert_eq!(glob_writes(&cold_events), Vec::<String>::new(), "cold run");

    let (warm, warm_events) = run_fresh_events(&ws).await?;
    assert_eq!(warm, cold);
    assert_eq!(glob_writes(&warm_events), Vec::<String>::new(), "warm run");
    Ok(())
}

/// `a_b.txt` and `a/b.txt` are distinct entries; flattening `/` to `_` in the
/// artifact name must not merge or reject them.
#[tokio::test]
async fn names_glob_stages_entries_whose_flattened_names_collide() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("pkg/src/a_b.txt", "one");
    ws.write_file("pkg/src/a/b.txt", "two");
    ws.write_build_file("pkg", NAMES_PATHS_BUILD);

    let out = run_fresh(&ws).await?;
    assert_eq!(out, "a/b.txt\na_b.txt\n");
    Ok(())
}

/// The staging-collision exemption for two fs deps holds within one fs mode
/// only, and it recognizes the mode exactly as the fs provider spells it: addrs
/// built by `pluginfs::glob_addr` / `pluginfs::names_glob_addr` over the same
/// file collide, while two content addrs over it do not.
#[tokio::test]
async fn names_and_content_globs_over_one_file_collide() -> anyhow::Result<()> {
    use heph::pluginfs::{glob_addr, names_glob_addr};

    let build = |a: &str, b: &str| {
        format!(
            r#"
target(
    name = "t",
    driver = "bash",
    deps = {{"a": ["{a}"], "b": ["{b}"]}},
    out = "out.txt",
    run = 'cat $SRC_A > $OUT',
)
"#
        )
    };
    let content = glob_addr("pkg/src/*", &[]).format();
    let content2 = glob_addr("pkg/src/*.txt", &[]).format();
    let names = names_glob_addr("pkg/src/*", &[]).format();

    let ws = Workspace::new();
    ws.write_file("pkg/src/a.txt", "one");

    ws.write_build_file("pkg", &build(&content, &content2));
    assert_eq!(run_fresh(&ws).await?, "one", "two content globs agree");

    ws.write_build_file("pkg", &build(&content, &names));
    let err = run_fresh(&ws)
        .await
        .expect_err("a names placeholder over a content file must collide");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("collision") && msg.contains("m=names"),
        "the error names the collision and the names glob: {msg}"
    );
    Ok(())
}
