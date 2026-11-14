//! `failure_out`: what a failed run leaves behind, kept in its failure slot as
//! `out/<workspace-relative path>`. Collected only on a process exit (a code
//! from 1 to 255 or a signal) or the run's own `timeout`, never on success or
//! cancellation; copied, never
//! linked; following no link out of the sandbox. Every test has its own
//! `TempDir` home.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::{self, Workspace};
use crate::failure_slot::{failures, files_containing, removed, render, slot_exists};
use heph::engine::failure_slot::{LOG, OUT};
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The slot's `out/` for `addr`.
fn out_dir(engine: &Engine, addr: &str) -> PathBuf {
    engine
        .failure_slots()
        .slot_dir(&parse_addr(addr).expect("addr"))
        .join(OUT)
}

/// The workspace root inside `//pkg:name`'s sandbox.
fn sandbox_ws(engine: &Engine, pkg: &str, name: &str) -> PathBuf {
    engine
        .home
        .join("sandbox")
        .join(pkg)
        .join(format!("__target_{name}"))
        .join("ws")
}

/// Whether a `failure_out` tar (`heph-collect-artifacts/failureout-*`) is
/// under the run's sandbox.
fn packs_failure_out(sandbox: &Path) -> bool {
    std::fs::read_dir(sandbox.join("heph-collect-artifacts")).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().as_bytes().starts_with(b"failureout-"))
    })
}

/// The names directly under `failures/`, slots and temporaries alike.
fn slot_root_names(engine: &Engine) -> Vec<String> {
    let root = engine.failure_slots().root();
    if !root.exists() {
        return Vec::new();
    }
    std::fs::read_dir(root)
        .expect("listing failures/")
        .map(|e| {
            e.expect("failures/ entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// One entry of a [`tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Dir,
    File(Vec<u8>),
    Link(Vec<u8>),
}

/// Everything under `dir`, keyed by the raw bytes of its path relative to
/// `dir`: byte-exact whatever the names are, UTF-8 or not. Links are recorded,
/// never followed.
fn tree(dir: &Path) -> BTreeMap<Vec<u8>, Entry> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<Vec<u8>, Entry>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries {
            let path = e.expect("dir entry").path();
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .as_os_str()
                .as_bytes()
                .to_vec();
            let md = std::fs::symlink_metadata(&path).expect("lstat");
            if md.is_symlink() {
                let to = std::fs::read_link(&path).expect("readlink");
                out.insert(rel, Entry::Link(to.as_os_str().as_bytes().to_vec()));
            } else if md.is_dir() {
                out.insert(rel, Entry::Dir);
                walk(root, &path, out);
            } else {
                out.insert(rel, Entry::File(std::fs::read(&path).expect("read")));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The relative paths of a [`tree`], lossily, for assertions on names.
fn names(tree: &BTreeMap<Vec<u8>, Entry>) -> Vec<String> {
    tree.keys()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .collect()
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    std::fs::read_to_string(path)
        .map_err(|e| format!("reading {}: {e}", path.display()))
        .expect("readable")
}

/// Wait until `cond` holds, or fail after `secs`.
async fn eventually(secs: u64, what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn collected_into_slot_on_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", failure_out = ["report.txt", "logs/*.log"],
       run = "mkdir -p logs; printf report-bytes > report.txt; printf one > logs/a.log; printf skip > logs/a.txt; echo log-marker; exit 3")"#,
    );
    let f = failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let out = out_dir(&ws.engine, "//p:t");
    assert_eq!(read(out.join("p/report.txt")), "report-bytes");
    assert_eq!(read(out.join("p/logs/a.log")), "one");
    assert_eq!(
        names(&tree(&out)),
        ["p", "p/logs", "p/logs/a.log", "p/report.txt"],
        "only the matches, at their workspace-relative paths"
    );
    // The log is kept beside `out/`, and the slot reads as complete.
    let slots = ws.engine.failure_slots();
    let addr = parse_addr("//p:t")?;
    assert_eq!(read(slots.slot_dir(&addr).join(LOG)), "log-marker\n");
    assert!(slots.read(&addr).is_some(), "the slot has its manifest");
    let failure = f
        .iter()
        .find(|f| f.addr == addr)
        .expect("the target's failure");
    assert_eq!(failure.outputs.as_deref(), Some(out.as_path()));
    assert!(
        !render(&f).contains("failure_out not kept"),
        "{}",
        render(&f)
    );
    // Unpacked out of the sandbox before it was removed, not raced by it.
    removed(sandbox_ws(&ws.engine, "p", "t").parent().expect("sandbox")).await;
    assert_eq!(read(out.join("p/report.txt")), "report-bytes");
    Ok(())
}

#[tokio::test]
async fn nothing_on_success() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", out = "o.txt", failure_out = "report.txt",
       run = "printf success-report-marker > report.txt; printf ok > o.txt")"#,
    );
    let res = ws.run("//p:t").await?;
    let paths = common::artifact_paths(&res);
    assert!(
        paths.iter().any(|p| p.ends_with("o.txt")),
        "the declared output is there: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("report.txt")),
        "a failure output never reaches a cache entry: {paths:?}"
    );
    assert!(!slot_exists(&ws.engine, "//p:t"));
    assert!(slot_root_names(&ws.engine).is_empty());
    assert!(
        !packs_failure_out(sandbox_ws(&ws.engine, "p", "t").parent().expect("sandbox")),
        "a pass packs nothing"
    );
    // Nowhere in the home but the run's own sandbox holds the bytes.
    let sandboxes = ws.engine.home.join("sandbox");
    let leaked: Vec<PathBuf> = files_containing(&ws.engine.home, "success-report-marker")
        .into_iter()
        .filter(|p| !p.starts_with(&sandboxes))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
    Ok(())
}

#[tokio::test]
async fn collected_on_signal() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", failure_out = "report.txt",
       run = "printf killed-report > report.txt; kill -9 $$")"#,
    );
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert_eq!(
        read(out_dir(&ws.engine, "//p:t").join("p/report.txt")),
        "killed-report"
    );
    Ok(())
}

#[tokio::test]
async fn collected_on_exit_137() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", failure_out = "report.txt",
       run = "printf oom-report > report.txt; exit 137")"#,
    );
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert_eq!(
        read(out_dir(&ws.engine, "//p:t").join("p/report.txt")),
        "oom-report"
    );
    Ok(())
}

/// A run stopped by its own `timeout` is a failure, not a cancellation: what
/// it wrote before the limit is kept.
#[tokio::test]
async fn collected_on_timeout() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", failure_out = "report.txt", timeout = 1,
       run = "printf timeout-report > report.txt; sleep 60")"#,
    );
    let f = failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert!(render(&f).contains("timed out after 1s"), "{}", render(&f));
    assert_eq!(
        read(out_dir(&ws.engine, "//p:t").join("p/report.txt")),
        "timeout-report"
    );
    Ok(())
}

/// Fail-fast cancels a running sibling: its process is killed, and that is a
/// cancellation, not a failure, so it keeps nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_on_cancel() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    let c = ctl.path().display();
    ws.write_build_file(
        "p",
        &format!(
            r#"
target(name = "slow", driver = "bash", cache = False, failure_out = "slow.txt",
       run = "printf slow > slow.txt; printf '' > {c}/started; while [ ! -f {c}/release ]; do sleep 0.05; done; printf '' > {c}/outlived; exit 1")
target(name = "fast", driver = "bash", cache = False, failure_out = "fast.txt",
       run = "while [ ! -f {c}/started ]; do sleep 0.05; done; printf fast > fast.txt; exit 1")
target(name = "grp", driver = "group", deps = [":slow", ":fast"])
"#
        ),
    );
    let rs = ws.engine.new_state();
    let res = tokio::time::timeout(
        Duration::from_secs(120),
        ws.engine.clone().result_addr(
            rs.clone(),
            &parse_addr("//p:grp")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        ),
    )
    .await
    .expect("fail-fast never cancelled the waiting sibling");
    assert!(res.is_err());
    drop(res);
    common::drain(rs).await;
    assert!(ctl.path().join("started").exists(), "the sibling ran");
    // A sibling process that outlived its cancellation would now exit 1 on its
    // own; without the release it would hold the runtime's shutdown open.
    std::fs::write(ctl.path().join("release"), "")?;
    // The cancelled sibling's execute may unwind after the request returned.
    // A fill happens under its execute lock, so once that lock is free again
    // any late (wrong) fill has landed.
    let slow = parse_addr("//p:slow")?;
    eventually(30, "the cancelled sibling's execute never let go", || {
        matches!(ws.engine.result_lock().try_lock_execute(&slow), Ok(Some(_)))
    })
    .await;
    if ctl.path().join("outlived").exists() {
        eprintln!("nothing_on_cancel: the cancelled sibling's process outlived its cancellation");
    }
    assert_eq!(
        read(out_dir(&ws.engine, "//p:fast").join("p/fast.txt")),
        "fast"
    );
    assert!(
        !slot_exists(&ws.engine, "//p:slow"),
        "a cancelled sibling keeps nothing"
    );
    Ok(())
}

/// The request is dropped and cancelled while its `failure_out` is being
/// collected. The collect and the slot fill each run as one blocking job that
/// finishes even when its caller is gone, so no `.tmp-` slot is ever left
/// behind, whichever of them the drop lands in.
///
/// A "no hang, nothing half-made" test, deterministic in its assertion, not in
/// where the drop lands: thousands of matches make the managed layer's collect
/// long enough that the drop, made as soon as the `failure_out` tar appears,
/// usually lands inside it, but a run that finished first is accepted too. The
/// engine's fill window (a few renames) is pinned down by an engine unit test,
/// not here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_collect_leaves_no_tmp_dir() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", cache = False, failure_out = "r",
       run = "mkdir -p r; for i in $(seq 1 4000); do printf x > r/$i.txt; done; exit 1")"#,
    );
    let sandbox = sandbox_ws(&ws.engine, "p", "t")
        .parent()
        .expect("sandbox")
        .to_path_buf();
    let root = ws.engine.failure_slots().root().to_path_buf();
    let has_tmp = |root: &Path| {
        std::fs::read_dir(root).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().as_bytes().starts_with(b".tmp-"))
        })
    };

    let rs = ws.engine.new_state();
    let task = tokio::spawn({
        let (engine, rs) = (ws.engine.clone(), rs.clone());
        let addr = parse_addr("//p:t")?;
        async move {
            engine
                .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
                .await
                .map(|_| ())
        }
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    // Whether the collect (or the fill after it) was seen running, rather
    // than the run finishing before the poll caught it.
    let seen_collect = loop {
        if packs_failure_out(&sandbox) || has_tmp(&root) {
            break true;
        }
        if task.is_finished() {
            break false;
        }
        assert!(Instant::now() < deadline, "the collect never started");
        tokio::time::sleep(Duration::from_millis(1)).await;
    };
    rs.ctoken().cancel();
    task.abort();
    // Aborted, or finished on its own: either way it is over.
    assert!(task.await.map_or(true, |res| res.is_err()));
    common::drain(rs).await;

    eventually(30, "a .tmp- slot outlived its fill", || !has_tmp(&root)).await;
    let slots = slot_root_names(&ws.engine);
    // A run that finished before the poll saw its collect was a plain failure,
    // never cancelled: it must have filled its slot. Without this, a run that
    // never reached the collect at all (a broken BUILD, a wrong staging path)
    // would leave nothing and pass the loop below on no slot.
    assert!(
        seen_collect || !slots.is_empty(),
        "the run finished without the collect ever being seen, and kept no slot"
    );
    // Whatever slot there is, is a complete one.
    for name in slots {
        assert!(!name.starts_with(".tmp-"), "{name}");
        assert!(
            ws.engine
                .failure_slots()
                .read(&parse_addr("//p:t")?)
                .is_some(),
            "a slot without its manifest: {name}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn missing_path_is_skipped() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"
target(name = "t", driver = "bash",
       failure_out = ["missing.txt", "gone/", "nomatch/**/*.x", "present.txt"],
       run = "printf here > present.txt; exit 1")
target(name = "none", driver = "bash", failure_out = ["missing.txt", "*.nope"],
       run = "echo none-marker; exit 1")
"#,
    );
    let f = failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let out = out_dir(&ws.engine, "//p:t");
    assert_eq!(names(&tree(&out)), ["p", "p/present.txt"]);
    assert_eq!(read(out.join("p/present.txt")), "here");
    assert!(
        !render(&f).contains("failure_out not kept"),
        "{}",
        render(&f)
    );

    // Nothing matched at all: the log slot as before, and no `out/`.
    let f = failures(&ws.engine, "//p:none", &ResultOptions::default()).await?;
    let addr = parse_addr("//p:none")?;
    let slot = ws.engine.failure_slots().slot_dir(&addr);
    assert_eq!(read(slot.join(LOG)), "none-marker\n");
    assert!(!slot.join(OUT).exists());
    let failure = f.iter().find(|f| f.addr == addr).expect("the failure");
    assert_eq!(failure.outputs, None);
    assert!(
        !render(&f).contains("failure_out not kept"),
        "{}",
        render(&f)
    );
    Ok(())
}

#[tokio::test]
async fn symlink_out_of_sandbox_not_copied() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    std::fs::write(ctl.path().join("secret.txt"), "secret-outside-marker")?;
    std::fs::create_dir(ctl.path().join("dir"))?;
    std::fs::write(ctl.path().join("dir/secret.txt"), "secret-outside-marker")?;
    let c = ctl.path().display();
    ws.write_build_file(
        "p",
        &format!(
            r#"target(name = "t", driver = "bash",
       failure_out = ["leak.txt", "leakdir", "viadir/secret.txt", "d", "ok.txt"],
       run = "ln -s {c}/secret.txt leak.txt; ln -s {c}/dir leakdir; ln -s {c}/dir viadir; mkdir -p d; printf in > d/kept.txt; ln -s {c}/secret.txt d/inner-leak; printf fine > ok.txt; exit 1")"#
        ),
    );
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let out = out_dir(&ws.engine, "//p:t");
    assert_eq!(
        names(&tree(&out)),
        ["p", "p/d", "p/d/kept.txt", "p/ok.txt"],
        "no link out of the sandbox is followed: not declared, not met in a \
         directory, not through a parent"
    );
    let slot = ws.engine.failure_slots().slot_dir(&parse_addr("//p:t")?);
    assert!(
        files_containing(&slot, "secret-outside-marker").is_empty(),
        "the outside bytes reached the slot"
    );
    Ok(())
}

/// A link loop, a two-link cycle, a dangling link, a link to a directory in
/// the sandbox, a link to the sandbox itself and one to `/`: collected without
/// hanging, and nothing outside the workspace is reached.
#[tokio::test]
async fn symlink_edge_cases() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash",
       failure_out = ["links/loop", "links/a", "links/dangling", "links/dirlink", "links/sandbox", "links/root", "links/**"],
       run = "mkdir -p links real/sub; printf inner > real/sub/f.txt; ln -s loop links/loop; ln -s b links/a; ln -s a links/b; ln -s missing links/dangling; ln -s ../real links/dirlink; ln -s ../../.. links/sandbox; ln -s / links/root; exit 1")"#,
    );
    tokio::time::timeout(
        Duration::from_secs(120),
        failures(&ws.engine, "//p:t", &ResultOptions::default()),
    )
    .await
    .expect("collecting the links hung")?;
    let out = out_dir(&ws.engine, "//p:t");
    // The link into the sandbox is copied as the directory it names; every
    // other link resolves nowhere, or out of the workspace, and is skipped.
    assert_eq!(
        names(&tree(&out)),
        [
            "p",
            "p/links",
            "p/links/dirlink",
            "p/links/dirlink/sub",
            "p/links/dirlink/sub/f.txt"
        ]
    );
    assert_eq!(read(out.join("p/links/dirlink/sub/f.txt")), "inner");
    Ok(())
}

#[tokio::test]
async fn directory_is_copied_whole() -> anyhow::Result<()> {
    // Kept: the slot is compared with the failed sandbox.
    let ws = Workspace::keeping_failed_sandboxes();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash",
       failure_out = {"report": ["test-results/"], "logs": ["tree/**/*.log"]},
       run = "mkdir -p test-results/a/b test-results/c test-results/empty tree/x/y; printf 1 > test-results/a/b/deep.txt; printf 2 > test-results/c/x.txt; printf 3 > test-results/top.txt; printf g > tree/top.log; printf g > tree/x/y/z.log; printf n > tree/x/y/z.txt; exit 1")"#,
    );
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let out = out_dir(&ws.engine, "//p:t");
    let sandbox = sandbox_ws(&ws.engine, "p", "t");
    let mut ran = tree(&sandbox.join("p/test-results"));
    assert_eq!(ran.len(), 7, "the run's tree: {:?}", names(&ran));
    // A tar holds files: a directory with no file under it is not kept.
    assert!(ran.remove(b"empty".as_slice()).is_some());
    assert_eq!(
        tree(&out.join("p/test-results")),
        ran,
        "the directory, subdirectories and all, but the empty one"
    );
    let logs = tree(&out.join("p/tree"));
    assert_eq!(
        names(&logs),
        ["top.log", "x", "x/y", "x/y/z.log"],
        "a nested glob keeps its matches only"
    );
    Ok(())
}

/// Names with spaces, non-ASCII and non-UTF-8 bytes, and a deep path: the
/// slot's tree is the run's tree, byte for byte, but for a non-UTF-8 name,
/// which a tar member cannot carry and is skipped (as an `out` path would be
/// mangled). Linux filesystems take the non-UTF-8 name; APFS refuses it
/// (`EILSEQ`), which the run records in `nonutf8-refused`, outside
/// `failure_out`.
#[tokio::test]
async fn odd_names() -> anyhow::Result<()> {
    // Kept: the slot is compared with the failed sandbox.
    let ws = Workspace::keeping_failed_sandboxes();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", failure_out = ["odd"],
       run = "mkdir -p 'odd/with space/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p'; printf s > 'odd/with space/file name.txt'; printf d > 'odd/with space/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/deep.txt'; printf u > 'odd/ünïcödé ✓.txt'; printf t > 'odd/tab\there'; printf n > $'odd/bad\\xffname' 2>/dev/null || printf '' > nonutf8-refused; exit 1")"#,
    );
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let pkg = sandbox_ws(&ws.engine, "p", "t").join("p");
    let ran = tree(&pkg.join("odd"));
    assert!(ran.len() >= 21, "the run wrote its tree: {:?}", names(&ran));
    assert!(ran.contains_key("ünïcödé ✓.txt".as_bytes()));
    let kept = tree(&out_dir(&ws.engine, "//p:t").join("p/odd"));
    let non_utf8: &[u8] = b"bad\xffname";
    let mut want = ran.clone();
    want.remove(non_utf8);
    assert_eq!(kept, want, "kept byte-exact, but for the non-UTF-8 name");

    let refused = pkg.join("nonutf8-refused").exists();
    if cfg!(target_os = "linux") {
        assert!(!refused, "the filesystem refused a non-UTF-8 name");
        assert_eq!(
            ran.get(non_utf8),
            Some(&Entry::File(b"n".to_vec())),
            "the run wrote the non-UTF-8 name: {:?}",
            names(&ran)
        );
        assert_eq!(kept.get(non_utf8), None, "and it was skipped");
    } else {
        assert!(refused, "APFS took a non-UTF-8 name: {:?}", names(&ran));
        assert!(
            ran.keys().all(|k| std::str::from_utf8(k).is_ok()),
            "{:?}",
            names(&ran)
        );
    }
    Ok(())
}

/// The same path as `out` and `failure_out`: a pass caches it as an output
/// and keeps nothing; a failure keeps it in the slot and caches nothing.
#[tokio::test]
async fn overlapping_out_is_independent() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    let c = ctl.path().display();
    ws.write_build_file(
        "p",
        &format!(
            r#"target(name = "t", driver = "bash", out = "report.txt", failure_out = "report.txt",
       run = "if [ -f {c}/fail ]; then printf fail-run-marker > report.txt; exit 1; fi; printf pass-run-marker > report.txt")"#
        ),
    );
    std::fs::write(ctl.path().join("fail"), "")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    assert_eq!(
        read(out_dir(&ws.engine, "//p:t").join("p/report.txt")),
        "fail-run-marker"
    );

    std::fs::remove_file(ctl.path().join("fail"))?;
    let res = ws.run("//p:t").await?;
    assert_eq!(common::artifact_string(&res), "pass-run-marker");
    assert!(
        !slot_exists(&ws.engine, "//p:t"),
        "the pass cleared the slot"
    );
    let (sandboxes, slots) = (
        ws.engine.home.join("sandbox"),
        ws.engine.failure_slots().root().to_path_buf(),
    );
    let leaked: Vec<PathBuf> = files_containing(&ws.engine.home, "fail-run-marker")
        .into_iter()
        .filter(|p| !p.starts_with(&sandboxes) && !p.starts_with(&slots))
        .collect();
    assert!(
        leaked.is_empty(),
        "the failure's bytes were cached: {leaked:?}"
    );
    Ok(())
}

/// A `read_only_deps` input is linked into the sandbox from the shared stage;
/// the slot gets a copy of it, never a link that would let an edit in the
/// slot change the staged input under its key (D5).
#[tokio::test]
async fn matched_input_is_copied_not_linked() -> anyhow::Result<()> {
    // Kept: the test reads the run's copy of the input in the failed sandbox.
    let ws = Workspace::keeping_failed_sandboxes();
    ws.write_build_file(
        "p",
        r#"
target(name = "data", driver = "bash", out = "ro.txt", run = "printf staged-input-marker > ro.txt")
target(name = "t", driver = "bash", deps = {"r": ":data"}, read_only_deps = ["r"],
       failure_out = "ro.txt", run = "exit 1")
"#,
    );
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let stage = ws.engine.home.join("stage");
    let staged: Vec<PathBuf> = files_containing(&stage, "staged-input-marker");
    assert!(!staged.is_empty(), "the input was staged: {staged:?}");
    // Non-vacuous: the run's copy of the input really is the stage's file.
    let in_sandbox = sandbox_ws(&ws.engine, "p", "t").join("p/ro.txt");
    let real = std::fs::metadata(&in_sandbox)?;
    assert!(
        staged
            .iter()
            .any(|s| std::fs::metadata(s)
                .is_ok_and(|m| (m.dev(), m.ino()) == (real.dev(), real.ino()))),
        "the input is linked from the stage"
    );

    let kept = out_dir(&ws.engine, "//p:t").join("p/ro.txt");
    let md = std::fs::symlink_metadata(&kept)?;
    assert!(md.is_file(), "a regular file, not a link");
    assert_eq!(read(&kept), "staged-input-marker");
    for s in &staged {
        let m = std::fs::metadata(s)?;
        assert_ne!(
            (m.dev(), m.ino()),
            (md.dev(), md.ino()),
            "the slot shares an inode with {}",
            s.display()
        );
    }
    Ok(())
}

/// A target's `ctl/fail` file makes it fail, writing `failure_out` matches
/// named after `ctl/fail`'s contents; without it the target passes.
fn toggled(ws: &Workspace, ctl: &Path) {
    let c = ctl.display();
    ws.write_build_file(
        "p",
        &format!(
            r#"target(name = "t", driver = "bash", out = "o.txt", cache = False, failure_out = "*.txt",
       run = "if [ -f {c}/fail ]; then n=$(cat {c}/fail); printf $n > $n.txt; printf $n > common.txt; exit 1; fi; printf ok > o.txt")"#
        ),
    );
}

#[tokio::test]
async fn cleared_when_target_next_passes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    toggled(&ws, ctl.path());
    std::fs::write(ctl.path().join("fail"), "one")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let out = out_dir(&ws.engine, "//p:t");
    assert_eq!(read(out.join("p/one.txt")), "one");
    std::fs::remove_file(ctl.path().join("fail"))?;
    ws.run("//p:t").await?;
    assert!(!out.exists(), "out/ outlived its slot");
    assert!(!slot_exists(&ws.engine, "//p:t"));
    assert!(slot_root_names(&ws.engine).is_empty());
    Ok(())
}

#[tokio::test]
async fn replaced_by_next_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    toggled(&ws, ctl.path());
    std::fs::write(ctl.path().join("fail"), "one")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    std::fs::write(ctl.path().join("fail"), "two")?;
    failures(&ws.engine, "//p:t", &ResultOptions::default()).await?;
    let out = out_dir(&ws.engine, "//p:t");
    assert_eq!(
        names(&tree(&out)),
        ["p", "p/common.txt", "p/two.txt"],
        "nothing of the first failure survives"
    );
    assert_eq!(read(out.join("p/common.txt")), "two");
    assert_eq!(
        slot_root_names(&ws.engine).len(),
        1,
        "one slot, no temporaries: {:?}",
        slot_root_names(&ws.engine)
    );
    Ok(())
}

/// A credential source's `failure_out` is kept (what a target lists there is
/// its author's to judge), its log is not written, and its sandbox is torn
/// down once the slot is filled.
#[tokio::test]
async fn kept_for_material() -> anyhow::Result<()> {
    kept_for_material_in(Workspace::new(), false).await
}

/// `--keep-failed-sandboxes` keeps no credential material: the material
/// producer's sandbox is torn down all the same, and the box says why.
#[tokio::test]
async fn kept_for_material_when_keeping_failed_sandboxes() -> anyhow::Result<()> {
    kept_for_material_in(Workspace::keeping_failed_sandboxes(), true).await
}

async fn kept_for_material_in(ws: Workspace, keeping: bool) -> anyhow::Result<()> {
    ws.write_build_file(
        "auth",
        r#"
target(name = "fetch", driver = "bash", out = {"credential": "cred.json"}, cache = False,
       failure_out = "report.txt",
       run = ["printf material-report > report.txt", "echo producer-marker", "exit 4"])
target(name = "t", driver = "auth.credential", sources = ["//auth:fetch"],
       present = {"env": {"T": "${token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );
    let f = failures(&ws.engine, "//app:a", &ResultOptions::default()).await?;
    let out = render(&f);
    if keeping {
        // Removed against the flag: the box says so, and why.
        assert!(
            out.contains("  sandbox: removed (credential material)\n"),
            "{out}"
        );
    } else {
        // Removed as any failed sandbox is, and not a choice to explain.
        assert!(!out.contains("sandbox:"), "{out}");
    }
    let addr = parse_addr("//auth:fetch")?;
    let slots = ws.engine.failure_slots();
    assert!(slots.read(&addr).is_some(), "the producer has a slot");
    let slot = slots.slot_dir(&addr);
    assert_eq!(
        read(slot.join(OUT).join("auth/report.txt")),
        "material-report"
    );
    assert!(
        !slot.join(LOG).exists(),
        "a material log never reaches the disk"
    );
    assert!(files_containing(&slot, "producer-marker").is_empty());
    let sandbox = ws.engine.home.join("sandbox/auth/__target_fetch");
    eventually(10, "the failed material sandbox was never removed", || {
        !sandbox.exists()
    })
    .await;
    // Still there once the sandbox is gone: a move, not a view of it.
    assert_eq!(
        read(slot.join(OUT).join("auth/report.txt")),
        "material-report"
    );
    Ok(())
}
