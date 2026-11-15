//! What a finished target produced, for a viewer's Outputs tab: the
//! `ResultOutputs` event an executed target and a cache hit both send before
//! their `ResultEnd`, the declared group names on `ExecuteStart`, and the
//! per-file rows `Engine::output_listing` and `Engine::failure_outputs` read
//! from the cache entry and the failure slot.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use hcore::events::OutputGroupSummary;
use heph::engine::event::BuildEventKind;
use heph::engine::{Engine, ListedFile, ListedKind, OutputListing, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use std::path::PathBuf;
use std::sync::Arc;

/// `addr` asked for `outputs` in a request with an event stream, and every
/// event of it, through its end.
async fn events(
    engine: &Arc<Engine>,
    addr: &str,
    outputs: OutputMatcher,
) -> anyhow::Result<(anyhow::Result<()>, Vec<BuildEventKind>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(engine)
        .result_addr(rs, &parse_addr(addr)?, outputs, &ResultOptions::default())
        .await
        .map(drop);
    let mut out = Vec::new();
    let collect = async {
        while let Some(ev) = rx.recv().await {
            out.push(ev.kind);
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), collect)
        .await
        .map_err(|_elapsed| anyhow::anyhow!("the request's stream never ended"))?;
    Ok((res, out))
}

/// The groups of `addr`'s `ResultOutputs`, checking it is sent once and
/// right before its `ResultEnd`.
fn result_outputs<'a>(events: &'a [BuildEventKind], addr: &str) -> &'a [OutputGroupSummary] {
    let mine: Vec<(usize, &'a [OutputGroupSummary])> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            BuildEventKind::ResultOutputs { addr: a, groups } if a == addr => {
                Some((i, groups.as_slice()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(mine.len(), 1, "one per target: {events:#?}");
    let (at, groups) = mine.into_iter().next().expect("one");
    let end = events
        .iter()
        .position(|e| matches!(e, BuildEventKind::ResultEnd { addr: a, .. } if a == addr))
        .expect("a ResultEnd");
    assert!(at < end, "sent before the target's ResultEnd");
    assert!(
        !events
            .iter()
            .skip(at)
            .take(end - at)
            .any(|e| matches!(e, BuildEventKind::ResultEnd { .. })),
        "right before it: no other target ends between them"
    );
    groups
}

fn group_names(groups: &[OutputGroupSummary]) -> Vec<&str> {
    groups.iter().map(|g| g.group.as_str()).collect()
}

fn rows(listing: &OutputListing, group: &str) -> Vec<(PathBuf, ListedKind, u64)> {
    listing
        .groups
        .iter()
        .find(|g| g.group == group)
        .expect("the group is listed")
        .files
        .iter()
        .map(|f| (f.path.clone(), f.kind.clone(), f.size))
        .collect()
}

/// A content hash as the listing renders it: 16 hex digits.
fn is_hash(h: Option<&str>) -> bool {
    h.is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

const BUILD: &str = r#"target(name = "t", driver = "bash",
    out = {"a": "a.txt", "b": "b/", "e": "empty/"},
    run = "printf aaaa > a.txt; mkdir -p b/sub empty; printf 1 > b/one.txt; printf 22 > b/sub/two.txt")"#;

/// A run sends the declared group names on `ExecuteStart` and every group,
/// with its stored size and hash, on `ResultOutputs`; a cache hit asked for
/// one group sends the same groups, restored from the entry; the listing
/// reads each file's path, size and content hash, and no row for a
/// directory that holds files.
#[tokio::test]
async fn executed_and_hit_send_result_outputs_and_the_listing_reads_files() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("p", BUILD);

    let (res, ran) = events(&ws.engine, "//p:t", OutputMatcher::All).await?;
    res?;
    let start = ran
        .iter()
        .find_map(|e| match e {
            BuildEventKind::ExecuteStart { output_groups, .. } => Some(output_groups),
            _ => None,
        })
        .expect("it ran");
    let mut declared = start.clone();
    declared.sort();
    assert_eq!(declared, ["a", "b", "e"], "the declared group names");
    let executed = result_outputs(&ran, "//p:t").to_vec();
    assert_eq!(group_names(&executed), ["a", "b", "e"]);
    for g in &executed {
        assert_eq!(g.artifacts, 1, "{g:?}");
        assert!(g.size.is_some_and(|s| s > 0), "a stored size: {g:?}");
        assert!(g.hash.as_ref().is_some_and(|h| !h.is_empty()), "{g:?}");
    }

    // A hit, asked for one group only: still every group of the entry.
    let (res, hit) = events(&ws.engine, "//p:t", OutputMatcher::Exact(vec!["a".into()])).await?;
    res?;
    assert!(
        !hit.iter()
            .any(|e| matches!(e, BuildEventKind::ExecuteStart { .. })),
        "a cache hit: {hit:#?}"
    );
    assert_eq!(
        result_outputs(&hit, "//p:t"),
        executed.as_slice(),
        "the hit restores what the run wrote"
    );

    let addr = parse_addr("//p:t")?;
    let listing = ws
        .engine
        .output_listing(&addr, None)?
        .expect("the entry is listed");
    assert_eq!(
        listing
            .groups
            .iter()
            .map(|g| (g.group.as_str(), g.size, g.hash.clone()))
            .collect::<Vec<_>>(),
        executed
            .iter()
            .map(|g| (g.group.as_str(), g.size.expect("size"), g.hash.clone()))
            .collect::<Vec<_>>(),
        "the listing's groups are the event's"
    );
    assert!(listing.groups.iter().all(|g| g.unavailable.is_none()));
    assert_eq!(
        rows(&listing, "a"),
        [(PathBuf::from("p/a.txt"), ListedKind::File, 4)]
    );
    assert_eq!(
        rows(&listing, "b"),
        [
            (PathBuf::from("p/b/one.txt"), ListedKind::File, 1),
            (PathBuf::from("p/b/sub/two.txt"), ListedKind::File, 2),
        ],
        "directories that hold files are not rows"
    );
    assert_eq!(
        rows(&listing, "e"),
        [],
        "the packer keeps no entry for an empty directory: its group is listed, with no rows"
    );
    let two: &ListedFile = listing
        .groups
        .iter()
        .flat_map(|g| &g.files)
        .find(|f| f.path.ends_with("two.txt"))
        .expect("two.txt");
    let one = listing
        .groups
        .iter()
        .flat_map(|g| &g.files)
        .find(|f| f.path.ends_with("one.txt"))
        .expect("one.txt");
    assert!(is_hash(two.hash.as_deref()) && is_hash(one.hash.as_deref()));
    assert_ne!(two.hash, one.hash, "a hash of each file's own bytes");

    // By input hash: the same entry; an unknown one is none.
    let same = ws
        .engine
        .output_listing(&addr, Some(&listing.hashin))?
        .expect("listed by hashin");
    assert_eq!(same, listing);
    assert_eq!(ws.engine.output_listing(&addr, Some("nope"))?, None);
    assert_eq!(
        ws.engine.output_listing(&parse_addr("//p:never")?, None)?,
        None
    );
    Ok(())
}

/// A failure sends no `ResultOutputs`; what its `failure_out` kept is read
/// from the failure slot, file by file.
#[tokio::test]
async fn a_failure_sends_none_and_its_kept_files_are_listed() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", out = "never.txt", failure_out = ["report.txt", "logs/"],
       run = "mkdir -p logs; printf report > report.txt; printf one > logs/a.log; exit 3")"#,
    );
    let addr = parse_addr("//p:t")?;
    assert_eq!(
        ws.engine.failure_outputs(&addr)?,
        None,
        "nothing failed yet"
    );

    let (res, evs) = events(&ws.engine, "//p:t", OutputMatcher::All).await?;
    assert!(res.is_err(), "the target fails");
    assert!(
        !evs.iter()
            .any(|e| matches!(e, BuildEventKind::ResultOutputs { .. })),
        "{evs:#?}"
    );
    let start = evs
        .iter()
        .find_map(|e| match e {
            BuildEventKind::ExecuteStart { output_groups, .. } => Some(output_groups),
            _ => None,
        })
        .expect("it ran");
    assert_eq!(start, &[""], "the unnamed group is declared");

    let kept = ws
        .engine
        .failure_outputs(&addr)?
        .expect("the failure is recorded");
    assert_eq!(
        kept.kept
            .iter()
            .map(|f| (f.path.clone(), f.kind.clone(), f.size))
            .collect::<Vec<_>>(),
        [
            (PathBuf::from("p/logs/a.log"), ListedKind::File, 3),
            (PathBuf::from("p/report.txt"), ListedKind::File, 6),
        ]
    );
    assert!(kept.kept.iter().all(|f| is_hash(f.hash.as_deref())));
    assert!(kept.fixes.is_empty(), "not an in_place run");
    assert_eq!(ws.engine.output_listing(&addr, None)?, None, "no entry");
    Ok(())
}
