#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for the `scratch` declaration driver.
//!
//! These go through the real `Engine` — provider, BUILD-file evaluation, driver
//! registry, `get_def` — rather than calling the driver directly, because what is
//! being tested is the *wiring*: that `driver = "scratch"` resolves at all, that
//! its config reaches the driver, and that a bad declaration fails where a BUILD
//! author will see it.
//!
//! The storage, mounting and locking a declaration eventually drives are not here;
//! a declaration is inert on its own. See `docs/SCRATCH.md`.

mod common;

use common::Workspace;
use heph::engine::{OutputMatcher, ResultOptions};
use std::sync::Arc;

/// `EResult` has no `Debug`, so `expect_err` will not compile. Unwrap the error
/// side explicitly instead.
fn expect_err<T>(r: anyhow::Result<T>, what: &str) -> anyhow::Error {
    match r {
        Ok(_) => panic!("{what}"),
        Err(e) => e,
    }
}

/// The driver is registered and a declaration resolves. Without this, everything
/// downstream fails with "driver not found" and no test says why.
#[tokio::test]
async fn a_scratch_declaration_resolves_through_the_engine() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"
target(
    name = "gocache",
    driver = "scratch",
    path = ".cache/go-build",
    env = "GOCACHE",
    access = "shared",
    version = "go1.23",
    remote = True,
)
"#,
    );

    let spec = ws.get_spec("//build:gocache").await?;
    assert_eq!(spec.driver, "scratch");
    Ok(())
}

/// An unknown field is rejected rather than ignored. This is the guard that makes
/// removing a declaration field safe: `platform` used to exist, and a workspace
/// still setting it must be told so rather than silently getting a cache keyed
/// differently than its author believes.
#[tokio::test]
async fn an_unknown_field_on_a_declaration_is_rejected() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", platform = "os_arch")"#,
    );
    let spec = ws.get_spec("//build:c").await?;
    let err = heph::pluginscratch::parse_declaration(&spec).expect_err("must reject");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("platform"),
        "the message must name the field: {msg}"
    );
    Ok(())
}

/// A declaration produces no artifacts, so resolving one yields an empty result
/// rather than an error. That is what makes it safe to reference from the graph.
#[tokio::test]
async fn a_declaration_produces_no_artifacts() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "cache")"#,
    );

    let result = ws.run("//build:c").await?;
    assert!(
        result.artifacts.is_empty(),
        "a scratch declaration must produce nothing, got {} artifacts",
        result.artifacts.len()
    );
    Ok(())
}

/// The whole point of declaring it as a target: two packages can each have a
/// `gocache` without agreeing on a naming convention, because the addr is the
/// identity.
#[tokio::test]
async fn two_packages_can_declare_the_same_name() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "go",
        r#"target(name = "cache", driver = "scratch", path = ".cache/go")"#,
    );
    ws.write_build_file(
        "rust",
        r#"target(name = "cache", driver = "scratch", path = ".cache/rust")"#,
    );

    let go = ws.get_spec("//go:cache").await?;
    let rust = ws.get_spec("//rust:cache").await?;
    assert_eq!(go.driver, "scratch");
    assert_eq!(rust.driver, "scratch");
    assert_ne!(go.addr.format(), rust.addr.format());
    Ok(())
}

/// A declaration without a `path` is the env-var-only form, not an incomplete
/// one: the cache is announced through its variable and never placed in the tree.
/// That is what lets a target whose output is a broad glob use one at all —
/// nothing is in the tree for the glob to collect.
#[tokio::test]
async fn a_declaration_without_a_path_is_env_var_only() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", env = "MYCACHE")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "**/*", scratch = ["//build:c"],
       run = ["echo \"$MYCACHE\" > o.txt"])"#,
    );

    // The `**/*` glob would be rejected against a mounted scratch; with no mount
    // there is nothing in the tree to collect.
    let out = common::artifact_string(&*ws.run("//app:a").await?);
    let path = out.trim();
    assert!(path.starts_with('/'), "the variable must be set: {path:?}");
    assert!(path.contains("/scratch/"), "{path:?}");
    // And the directory really is absent from the sandbox tree.
    assert!(!out.contains("MYCACHE="), "{out:?}");
    Ok(())
}

/// A mount is a symlink out of the sandbox. An absolute path would let a BUILD
/// file place it anywhere on the machine, so it is rejected at the declaration
/// rather than at each use.
#[tokio::test]
async fn an_absolute_path_is_rejected_at_the_declaration() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "/var/tmp/cache")"#,
    );

    let err = expect_err(
        ws.run("//build:c").await,
        "an absolute scratch path must not resolve",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("relative"), "error must explain why: {msg}");
    // The addr belongs in the message: a workspace with many declarations needs
    // to know which one is wrong.
    assert!(
        msg.contains("//build:c"),
        "error must name the target: {msg}"
    );
    Ok(())
}

/// An unknown `access` is a typo, not a request for new behaviour. Saying what the
/// two options *mean* is the difference between a one-line fix and a docs hunt.
#[tokio::test]
async fn an_unknown_access_names_the_valid_options() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "cache", access = "concurrent")"#,
    );

    let err = expect_err(
        ws.run("//build:c").await,
        "an unknown access must not resolve",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("exclusive") && msg.contains("shared"), "{msg}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Referencing a scratch from a consuming target.
// ---------------------------------------------------------------------------

/// Read a target's def hash — the thing that must not move. Asserting on
/// `hashout` instead would be a trap: a target whose key moved still produces
/// identical bytes, so a hashout comparison passes while the cache misses on
/// every run.
async fn def_hash(engine: &Arc<heph::engine::Engine>, addr: &str) -> anyhow::Result<Vec<u8>> {
    let addr = heph::htaddr::parse_addr(addr)?;
    let def = Arc::clone(engine)
        .get_def(engine.new_state(), &addr)
        .await?;
    Ok(def.target_def.hash.clone())
}

/// The central property (`docs/SCRATCH.md`, "The contract"): **nothing about a scratch
/// reaches a consumer's `hashin`**. Not the reference, and not the declaration.
///
/// The tempting design is the opposite — fold the declaration in, so bumping
/// `version` rebuilds everything using the cache. But a target's outputs are
/// required to be identical whether its scratch is warm, cold, or absent, so a
/// fresh slot changes nothing about them and the rebuild is pure waste. The
/// `version` bump is the case that is easy to get wrong, so it is asserted
/// separately below.
#[tokio::test]
async fn referencing_a_scratch_does_not_change_the_consumer_hash() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "bare", driver = "bash", run = "echo hi > $OUT", out = "o.txt")
target(name = "with", driver = "bash", run = "echo hi > $OUT", out = "o.txt",
       scratch = ["//build:c"])
target(name = "other", driver = "bash", run = "echo bye > $OUT", out = "o.txt")"#,
    );

    let engine = ws.reopen()?;
    // Precondition: `def_hash` discriminates. Without this the assertion below
    // would pass just as happily if it returned a constant, and a test that
    // cannot fail proves nothing.
    assert_ne!(
        def_hash(&engine, "//app:bare").await?,
        def_hash(&engine, "//app:other").await?,
        "def_hash must distinguish targets that genuinely differ"
    );
    assert_eq!(
        def_hash(&engine, "//app:bare").await?,
        def_hash(&engine, "//app:with").await?,
        "a scratch reference must not change the consumer's def hash"
    );
    // And end to end: it does not change what the target produces either.
    let bare = ws.run("//app:bare").await?;
    let with = ws.run("//app:with").await?;
    assert_eq!(bare.artifacts[0].hashout()?, with.artifacts[0].hashout()?);
    Ok(())
}

/// Bumping a scratch's `version` yields a fresh, empty slot and must leave every
/// consumer's cached result a hit — which is exactly what you want when the reason
/// for bumping it is "the old cache had gone bad".
#[tokio::test]
async fn bumping_a_scratch_version_does_not_invalidate_consumers() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let consumer = r#"target(name = "a", driver = "bash", run = "echo hi > $OUT", out = "o.txt",
       scratch = ["//build:c"])"#;
    ws.write_build_file("app", consumer);

    let decl = |v: &str| {
        format!(r#"target(name = "c", driver = "scratch", path = ".cache/x", version = "{v}")"#)
    };

    ws.write_build_file("build", &decl("v1"));
    let first_engine = ws.reopen()?;
    let first_def_hash = def_hash(&first_engine, "//app:a").await?;
    let first = ws.run("//app:a").await?;
    let first_hash = first.artifacts[0].hashout()?;
    // Released before the second engine opens — see the note in
    // `a_scratch_carries_state_between_runs`.
    drop(first);

    ws.write_build_file("build", &decl("v2"));
    // A second engine over the same on-disk cache — what the next `heph`
    // invocation sees. Needed because a spec is memoized per engine, so rewriting
    // the BUILD file and re-running through the same one would replay the old
    // declaration and prove nothing.
    let engine = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let second = engine
        .clone()
        .result_addr(
            engine.new_state(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    // The load-bearing assertion: the consumer's *key* is untouched, so its
    // cached result is still a hit. A `version` bump gives a fresh empty slot and
    // rebuilds nothing.
    assert_eq!(
        first_def_hash,
        def_hash(&engine, "//app:a").await?,
        "a scratch `version` bump must not change a consumer's def hash"
    );
    assert_eq!(first_hash, second.artifacts[0].hashout()?);
    Ok(())
}

/// Pointing `scratch` at something that is not a scratch target is a BUILD-file
/// mistake that would otherwise show up much later as a mount doing nothing. Both
/// ends belong in the message: the author is reading the consumer, and the problem
/// is the thing it named.
#[tokio::test]
async fn referencing_a_non_scratch_target_names_both_ends() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "dep", driver = "bash", run = "echo x > $OUT", out = "o.txt")
target(name = "a", driver = "bash", run = "echo hi > $OUT", out = "o.txt",
       scratch = ["//app:dep"])"#,
    );

    let err = expect_err(
        ws.run("//app:a").await,
        "a non-scratch ref must not resolve",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("//app:a"), "must name the consumer: {msg}");
    assert!(msg.contains("//app:dep"), "must name the referent: {msg}");
    assert!(msg.contains("deps"), "must suggest the likely fix: {msg}");
    Ok(())
}

/// Two caches claiming one variable means one silently shadows the other, so
/// neither is safe to mount.
#[tokio::test]
async fn two_scratches_claiming_one_env_var_is_an_error() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "a", driver = "scratch", path = "ca", env = "GOCACHE")
target(name = "b", driver = "scratch", path = "cb", env = "GOCACHE")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "x", driver = "bash", run = "echo hi > $OUT", out = "o.txt",
       scratch = ["//build:a", "//build:b"])"#,
    );

    let err = expect_err(ws.run("//app:x").await, "an env collision must not resolve");
    let msg = format!("{err:#}");
    assert!(msg.contains("GOCACHE"), "{msg}");
    assert!(
        msg.contains("//build:a") && msg.contains("//build:b"),
        "{msg}"
    );
    Ok(())
}

/// One cache mounted inside another means whatever writes the outer also writes
/// the inner — two caches over one set of bytes, with policies that disagree.
#[tokio::test]
async fn overlapping_mounts_are_an_error() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "outer", driver = "scratch", path = ".cache")
target(name = "inner", driver = "scratch", path = ".cache/go")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "x", driver = "bash", run = "echo hi > $OUT", out = "o.txt",
       scratch = ["//build:outer", "//build:inner"])"#,
    );

    let err = expect_err(
        ws.run("//app:x").await,
        "overlapping mounts must not resolve",
    );
    assert!(format!("{err:#}").contains("overlap"));
    Ok(())
}

/// Referencing the same scratch twice mounts one directory twice and sets one
/// variable twice. Quietly collapsing it would hide a BUILD-file mistake.
#[tokio::test]
async fn referencing_one_scratch_twice_is_an_error() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "cache")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "x", driver = "bash", run = "echo hi > $OUT", out = "o.txt",
       scratch = ["//build:c", "//build:c"])"#,
    );

    let err = expect_err(ws.run("//app:x").await, "a duplicate ref must not resolve");
    assert!(format!("{err:#}").contains("twice"));
    Ok(())
}

/// Many targets sharing one declaration is the whole point, and it must resolve
/// without any of them configuring anything.
#[tokio::test]
async fn many_targets_can_share_one_declaration() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", access = "shared")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt",
       scratch = ["//build:c"])
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt",
       scratch = ["//build:c"])"#,
    );

    assert!(!ws.run("//app:a").await?.artifacts.is_empty());
    assert!(!ws.run("//app:b").await?.artifacts.is_empty());
    Ok(())
}

/// Serialization is the reason `access` exists: two targets sharing one cache
/// must not be inside it at the same time, or a tool that assumes sole ownership
/// of its cache directory corrupts it. Nothing else in this file asserts it —
/// every other test runs one target at a time, where a lock that was never taken
/// looks identical to one that was.
///
/// Each target refuses to proceed if it finds the other's in-progress marker, so
/// the assertion is on the targets' own observation rather than on timing.
#[tokio::test]
async fn exclusive_targets_never_share_the_directory() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C",
       access = "exclusive")"#,
    );
    // Claim, linger, release. `mkdir` is the atomic test-and-set every shell
    // has: it fails if the directory is already there, so a target that finds
    // the marker knows another one is inside the cache right now.
    let t = |name: &str| {
        format!(
            r#"target(name = "{name}", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = [
         "if ! mkdir \"$C/busy\" 2>/dev/null; then echo OVERLAP > $OUT; exit 0; fi",
         "sleep 0.3",
         "rmdir \"$C/busy\"",
         "echo alone > $OUT",
       ])"#
        )
    };
    ws.write_build_file("app", &format!("{}\n{}", t("a"), t("b")));

    let (a, b) = tokio::join!(ws.run("//app:a"), ws.run("//app:b"));
    let (a, b) = (a?, b?);
    let (sa, sb) = (
        common::artifact_string(&a).trim().to_string(),
        common::artifact_string(&b).trim().to_string(),
    );
    drop((a, b));
    assert_eq!(
        (sa.as_str(), sb.as_str()),
        ("alone", "alone"),
        "two `exclusive` consumers of one cache overlapped"
    );
    Ok(())
}

/// `shared` is the "trust the tool" escape hatch — Go's build cache is safe under
/// concurrent use, and forcing those targets through one lock would serialize a
/// whole build for nothing. Both must complete; the point is that neither is
/// blocked, not that they overlap (asserting overlap would be a race).
#[tokio::test]
async fn shared_targets_are_not_serialized_against_each_other() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C",
       access = "shared")"#,
    );
    let t = |name: &str| {
        format!(
            r#"target(name = "{name}", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = ["echo {name} > \"$C/{name}\"", "echo ok > $OUT"])"#
        )
    };
    ws.write_build_file("app", &format!("{}\n{}", t("a"), t("b")));

    let (a, b) = tokio::join!(ws.run("//app:a"), ws.run("//app:b"));
    let (a, b) = (a?, b?);
    assert_eq!(common::artifact_string(&a).trim(), "ok");
    assert_eq!(common::artifact_string(&b).trim(), "ok");
    Ok(())
}

// ---------------------------------------------------------------------------
// Mounting: the point of the whole thing.
// ---------------------------------------------------------------------------

/// The feature, end to end: a target writes into its scratch, a later run with
/// changed inputs (so it genuinely re-executes) reads back what the first wrote.
///
/// This is the property everything else exists to support — state carried between
/// runs — and it is asserted through the real sandbox, symlink and env var.
#[tokio::test]
async fn a_scratch_carries_state_between_runs() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "MYCACHE")"#,
    );
    let build = |marker: &str| {
        format!(
            r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = [
         "if [ -f \"$MYCACHE/marker\" ]; then cat \"$MYCACHE/marker\" > $OUT; else echo cold > $OUT; fi",
         "echo {marker} > \"$MYCACHE/marker\"",
       ])"#
        )
    };

    ws.write_build_file("app", &build("first"));
    let first = ws.run("//app:a").await?;
    let first_out = common::artifact_string(&first).trim().to_string();
    // Drop before reopening. An `EResult`'s artifacts hold a *riding read guard*
    // on their addr's result lock, and the second engine takes the **write** lock
    // on the same lock files to re-execute — so holding this across `reopen()`
    // deadlocks the two engines against each other. (A second run that were a
    // cache *hit* would only take a read lock and would not notice.) Same idiom
    // as `engine_core.rs`.
    drop(first);
    assert_eq!(first_out, "cold", "the first run must see an empty scratch");

    // Change the target so the second run is a genuine miss, not a cache hit —
    // a hit would never reach the sandbox and the test would prove nothing.
    ws.write_build_file("app", &build("second"));
    let engine = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let second = engine
        .clone()
        .result_addr(
            engine.new_state(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    assert_eq!(
        common::artifact_string(&second).trim(),
        "first",
        "the second run must see what the first wrote into the scratch"
    );
    Ok(())
}

/// `max_size` was accepted on the declaration and enforced nowhere — an author
/// could bound a cache, believe it bounded, and watch it grow without limit. The
/// cap drops the lineage **whole** rather than trimming it: heph cannot tell
/// which of a foreign tool's entries are hot, so evicting a guess would quietly
/// degrade the cache while claiming to manage it.
#[tokio::test]
async fn a_scratch_over_its_max_size_is_dropped_whole() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "MYCACHE",
       max_size = "4KiB")"#,
    );
    // Writes ~64KiB, well past the cap, and reports whether anything survived
    // from the run before it.
    let build = |marker: &str| {
        format!(
            r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = [
         "if [ -f \"$MYCACHE/marker\" ]; then cat \"$MYCACHE/marker\" > $OUT; else echo cold > $OUT; fi",
         "echo {marker} > \"$MYCACHE/marker\"",
         "printf '%*s' 65536 '' > \"$MYCACHE/bulk\"",
       ])"#
        )
    };

    ws.write_build_file("app", &build("first"));
    let first = ws.run("//app:a").await?;
    let first_out = common::artifact_string(&first).trim().to_string();
    drop(first);
    assert_eq!(first_out, "cold");

    ws.write_build_file("app", &build("second"));
    let engine = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let second = engine
        .clone()
        .result_addr(
            engine.new_state(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    assert_eq!(
        common::artifact_string(&second).trim(),
        "cold",
        "a cache past its cap must be dropped, so the next run starts cold"
    );
    Ok(())
}

/// The cap must not fire on a cache that is merely non-empty — a cap that drops
/// everything is indistinguishable from no cache at all, and would make the
/// feature silently useless rather than loudly broken.
#[tokio::test]
async fn a_scratch_under_its_max_size_is_kept() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "MYCACHE",
       max_size = "1GiB")"#,
    );
    let build = |marker: &str| {
        format!(
            r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = [
         "if [ -f \"$MYCACHE/marker\" ]; then cat \"$MYCACHE/marker\" > $OUT; else echo cold > $OUT; fi",
         "echo {marker} > \"$MYCACHE/marker\"",
       ])"#
        )
    };

    ws.write_build_file("app", &build("first"));
    let first = ws.run("//app:a").await?;
    drop(first);

    ws.write_build_file("app", &build("second"));
    let engine = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let second = engine
        .clone()
        .result_addr(
            engine.new_state(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    assert_eq!(
        common::artifact_string(&second).trim(),
        "first",
        "a cache inside its cap must survive"
    );
    Ok(())
}

/// The declaration's `env` is what makes a reference sufficient: the consumer
/// wires nothing, and the variable holds the canonical slot path — an absolute
/// path outside the sandbox, not the in-sandbox mount. Tools bake absolute paths
/// into their cache entries, so every consumer must see the same string.
#[tokio::test]
async fn the_env_var_carries_the_canonical_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "MYCACHE")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = ["echo \"$MYCACHE\" > $OUT"])"#,
    );

    let out = common::artifact_string(&*ws.run("//app:a").await?);
    let path = out.trim();
    assert!(path.starts_with('/'), "must be absolute, got {path:?}");
    assert!(
        path.contains("/scratch/"),
        "must point into the scratch store, got {path:?}"
    );
    assert!(
        !path.contains("/sandbox/"),
        "must be the canonical slot, not the in-sandbox mount, got {path:?}"
    );
    Ok(())
}

/// Two targets referencing one declaration must resolve to one directory —
/// otherwise "sharing" is a word rather than a behaviour.
#[tokio::test]
async fn two_targets_sharing_a_declaration_get_one_directory() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "MYCACHE",
       access = "shared")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "a.txt", scratch = ["//build:c"],
       run = ["echo \"$MYCACHE\" > $OUT"])
target(name = "b", driver = "bash", out = "b.txt", scratch = ["//build:c"],
       run = ["echo \"$MYCACHE\" > $OUT"])"#,
    );

    let a = common::artifact_string(&*ws.run("//app:a").await?);
    let b = common::artifact_string(&*ws.run("//app:b").await?);
    assert_eq!(a.trim(), b.trim(), "one declaration is one slot");
    Ok(())
}

/// Two declarations differing only in `version` are different caches — that is
/// what makes `version` a bust handle rather than a label.
/// heph contributes nothing to a slot's identity beyond the addr and `version`.
/// Everything else on the declaration is policy about how a cache is *used*, not
/// a statement about what is in it, so none of it may split the slot — otherwise
/// changing where a cache mounts, or who may hold it, silently starts a new one.
#[tokio::test]
async fn identity_is_the_addr_and_version_and_nothing_else() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C",
       access = "shared", remote = True)"#,
    );
    let engine = ws.reopen()?;
    let a = slot_of(&engine, "//build:c").await?;

    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/y", env = "D",
       access = "exclusive")"#,
    );
    let engine = ws.reopen()?;
    let b = slot_of(&engine, "//build:c").await?;

    assert_eq!(
        a, b,
        "path/env/access/remote are policy, not identity — they must not split the slot"
    );
    Ok(())
}

/// Resolve a declaration's slot the way the engine does.
async fn slot_of(engine: &Arc<heph::engine::Engine>, addr: &str) -> anyhow::Result<String> {
    let addr = heph::htaddr::parse_addr(addr)?;
    let rs = engine.new_state();
    let spec = Arc::clone(engine).get_spec(rs, &addr).await?;
    let def = heph::pluginscratch::parse_declaration(&spec)?;
    Ok(heph::engine::scratch::ResolvedScratch { addr, def }.slot())
}

#[tokio::test]
async fn version_selects_a_different_slot() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "v1", driver = "scratch", path = ".cache/x", env = "C", version = "1")
target(name = "v2", driver = "scratch", path = ".cache/x", env = "C", version = "2")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:v1"],
       run = ["echo \"$C\" > $OUT"])
target(name = "b", driver = "bash", out = "o.txt", scratch = ["//build:v2"],
       run = ["echo \"$C\" > $OUT"])"#,
    );

    let a = common::artifact_string(&*ws.run("//app:a").await?);
    let b = common::artifact_string(&*ws.run("//app:b").await?);
    assert_ne!(
        a.trim(),
        b.trim(),
        "a `version` bump must give a fresh slot"
    );
    Ok(())
}

/// A scratch mounting where an input already landed would let the target read
/// cache contents where it believes it reads a declared dependency — bytes no
/// `hashin` describes. That is the one way a scratch can cause a *wrong build*
/// rather than a slow one, so it must fail rather than silently win.
#[tokio::test]
async fn a_scratch_cannot_mount_over_a_materialized_input() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "depdir", env = "C")"#,
    );
    // The producer's output unpacks into the consumer's sandbox at `depdir/f.txt`,
    // which is exactly where the scratch wants to mount.
    ws.write_build_file(
        "app",
        r#"target(name = "producer", driver = "bash", out = "depdir/f.txt",
       run = ["mkdir -p depdir && echo real > depdir/f.txt"])
target(name = "a", driver = "bash", out = "o.txt",
       scratch = ["//build:c"],
       deps = {"src": ["//app:producer"]},
       run = ["cat depdir/f.txt > $OUT"])"#,
    );

    let err = expect_err(
        ws.run("//app:a").await,
        "a scratch must not mount over a materialized input",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("already there"),
        "must explain the collision: {msg}"
    );
    assert!(msg.contains("//build:c"), "must name the scratch: {msg}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Lineages: one cache per branch, with a fallback.
// ---------------------------------------------------------------------------

/// Switching branches must not hand the arriving branch the departing branch's
/// cache state, and must not hand it back mutated. Each lineage keeps its own
/// directory; a new one is *seeded* from its fallback, so a switch costs a copy
/// rather than a rebuild.
///
/// The whole story in one test: build on `master`, switch to `feat`, and see
/// `master`'s work — then confirm `feat`'s own writes never reached `master`.
#[tokio::test]
async fn a_branch_switch_seeds_from_the_fallback_and_writes_stay_put() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C")"#,
    );
    // Reads whatever is in the cache, then stamps its own name over it.
    let build = |m: &str| {
        format!(
            r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = ["cat \"$C/marker\" > $OUT || echo cold > $OUT", "echo {m} > \"$C/marker\""])"#
        )
    };
    let run = async |engine: &Arc<heph::engine::Engine>| -> anyhow::Result<String> {
        let addr = heph::htaddr::parse_addr("//app:a")?;
        let opts = ResultOptions::default();
        let r = Arc::clone(engine)
            .result_addr(engine.new_state(), &addr, OutputMatcher::All, &opts)
            .await?;
        let out = common::artifact_string(&r).trim().to_string();
        drop(r); // releases the riding read lock before the next engine opens
        Ok(out)
    };

    let on = |scope: &str| heph::engine::ScratchOptions {
        scope: scope.to_string(),
        restore_scopes: vec!["master".to_string()],
        seed_on_fork: true,
    };

    // On `master`: cold, then leaves "master" behind.
    ws.write_build_file("app", &build("master"));
    assert_eq!(run(&ws.reopen_scoped(on("master"))?).await?, "cold");

    // Switch to `feat`. Its lineage is new, so it seeds from `master` — and sees
    // what `master` left, rather than starting cold.
    ws.write_build_file("app", &build("feat"));
    assert_eq!(
        run(&ws.reopen_scoped(on("feat"))?).await?,
        "master",
        "a new branch must seed from its fallback, not start cold"
    );

    // Back on `master`: it must still see its own work, not the branch's. This is
    // the isolation half — a branch cannot advance or corrupt what it forked from.
    ws.write_build_file("app", &build("master2"));
    assert_eq!(
        run(&ws.reopen_scoped(on("master"))?).await?,
        "master",
        "the branch's writes must not have reached the lineage it forked from"
    );
    Ok(())
}

/// With seeding off, a new lineage starts cold rather than paying a copy. The
/// knob exists for a large slot on a filesystem with no reflink.
#[tokio::test]
async fn seed_on_fork_off_starts_a_new_branch_cold() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C")"#,
    );
    let build = |m: &str| {
        format!(
            r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = ["cat \"$C/marker\" > $OUT || echo cold > $OUT", "echo {m} > \"$C/marker\""])"#
        )
    };
    let run = async |engine: &Arc<heph::engine::Engine>| -> anyhow::Result<String> {
        let addr = heph::htaddr::parse_addr("//app:a")?;
        let opts = ResultOptions::default();
        let r = Arc::clone(engine)
            .result_addr(engine.new_state(), &addr, OutputMatcher::All, &opts)
            .await?;
        let out = common::artifact_string(&r).trim().to_string();
        drop(r);
        Ok(out)
    };
    let on = |scope: &str, seed: bool| heph::engine::ScratchOptions {
        scope: scope.to_string(),
        restore_scopes: vec!["master".to_string()],
        seed_on_fork: seed,
    };

    ws.write_build_file("app", &build("master"));
    assert_eq!(run(&ws.reopen_scoped(on("master", false))?).await?, "cold");

    ws.write_build_file("app", &build("feat"));
    assert_eq!(
        run(&ws.reopen_scoped(on("feat", false))?).await?,
        "cold",
        "with seeding off a new lineage must not inherit"
    );
    Ok(())
}

/// `--scratch=off` is the audit mode for the whole contract: a target's outputs
/// must be identical whether its scratch is warm, cold or absent. With it, a
/// declared cache is not merely empty — the entire path is skipped, so nothing
/// resolves, locks, or mounts.
#[tokio::test]
async fn no_scratch_withholds_carried_over_state_not_the_directory() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C")"#,
    );
    // Reports what it *found in* the cache — which is the only thing the audit
    // withholds. Note `$C` is read unguarded: under the audit the variable is
    // still set, so a target does not have to defend against its own cache
    // vanishing.
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = [
         "if [ -f \"$C/marker\" ]; then cat \"$C/marker\" > $OUT; else echo cold > $OUT; fi",
         "echo warm > \"$C/marker\"",
       ])"#,
    );

    let run = async |engine: &Arc<heph::engine::Engine>,
                     force: bool,
                     no_scratch: bool|
           -> anyhow::Result<String> {
        let addr = heph::htaddr::parse_addr("//app:a")?;
        let opts = ResultOptions {
            force,
            no_scratch,
            ..Default::default()
        };
        let r = Arc::clone(engine)
            .result_addr(engine.new_state(), &addr, OutputMatcher::All, &opts)
            .await?;
        let out = common::artifact_string(&r).trim().to_string();
        drop(r);
        Ok(out)
    };

    // First run: nothing carried over yet, and it leaves a marker behind.
    assert_eq!(run(&ws.reopen()?, false, false).await?, "cold");
    // Second, forced: the marker is there, so state does carry between runs.
    assert_eq!(run(&ws.reopen()?, true, false).await?, "warm");

    // The trap this test originally fell into, and the reason the implication
    // now lives in the engine rather than at each call site: scratch never
    // reaches `hashin`, so without a rebuild an audit run is a plain cache hit
    // that replays the result built *with* a warm cache. It would pass by
    // reading exactly the answer it is meant to re-derive.
    //
    // Asserted with `force: false` on purpose. Every command that runs targets
    // — `run`, `inspect hashout`, `inspect outputs` — gets the rebuild from
    // `no_scratch` alone, so none of them can produce a vacuous audit by
    // forgetting to pair the two.
    assert_eq!(
        run(&ws.reopen()?, false, true).await?,
        "cold",
        "`--no-scratch` must imply a rebuild without the caller asking for one"
    );

    // The cache is still set up as normal — variable set, directory mounted —
    // and only its *contents* are withheld. The target reads `$C` unguarded and
    // still runs, which is the whole point of auditing the contract rather than
    // the target's shell.
    assert_eq!(
        run(&ws.reopen()?, true, true).await?,
        "cold",
        "--no-scratch must withhold carried-over state, not the directory"
    );

    // And it must not have disturbed the real lineage: the next ordinary run
    // still finds what the first one left.
    assert_eq!(run(&ws.reopen()?, true, false).await?, "warm");
    Ok(())
}

/// The audit must not disturb the stored cache — not its contents, not its
/// bookkeeping, not its existence. `--no-scratch` is a diagnostic, and a
/// diagnostic that mutates what it inspects is not one.
#[tokio::test]
async fn an_audit_leaves_the_stored_cache_untouched() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C")"#,
    );
    let build = |marker: &str| {
        format!(
            r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = ["echo hi > $OUT", "echo {marker} > \"$C/marker\""])"#
        )
    };
    ws.write_build_file("app", &build("first"));
    ws.run("//app:a").await?;

    let engine = ws.reopen()?;
    let slot = engine.scratch_slots()?[0].slot.clone();
    let head = heph::engine::scratch_remote::scope_head_dir(&engine.home, &slot, "");
    let before = std::fs::read_to_string(head.join("marker"))?;
    assert_eq!(before.trim(), "first");
    drop(engine);

    // Audit, forced so it genuinely re-executes.
    ws.write_build_file("app", &build("second"));
    let audit = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let r = Arc::clone(&audit)
        .result_addr(
            audit.new_state(),
            &addr,
            OutputMatcher::All,
            &ResultOptions {
                force: true,
                no_scratch: true,
                ..Default::default()
            },
        )
        .await?;
    drop(r);

    // The stored head still holds what the *first* run wrote. The audit wrote
    // "second" somewhere, and that somewhere was not here.
    assert_eq!(
        std::fs::read_to_string(head.join("marker"))?.trim(),
        "first",
        "the audit must not write into the stored cache"
    );
    assert!(
        !engine_slots_empty(&audit)?,
        "and must not remove the slot either"
    );
    Ok(())
}

fn engine_slots_empty(engine: &Arc<heph::engine::Engine>) -> anyhow::Result<bool> {
    Ok(engine.scratch_slots()?.is_empty())
}

/// The store describes itself, so it can be listed and reclaimed without reading
/// a single BUILD file — the property that keeps a cache manageable after the
/// target that made it is gone.
#[tokio::test]
async fn the_store_lists_and_reclaims_what_a_build_created() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "gocache", driver = "scratch", path = ".cache/x", env = "C",
       access = "shared")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:gocache"],
       run = ["echo hi > $OUT", "dd if=/dev/zero of=\"$C/blob\" bs=1024 count=8 2>/dev/null"])"#,
    );
    ws.run("//app:a").await?;

    let slots = ws.engine.scratch_slots()?;
    assert_eq!(slots.len(), 1, "the build must have created one slot");
    let meta = slots[0].meta.as_ref().expect("slot describes itself");
    assert_eq!(meta.addr, "//build:gocache");
    assert_eq!(meta.access, "shared");
    assert!(slots[0].bytes >= 8 * 1024, "got {} bytes", slots[0].bytes);

    // Named removal finds it by the addr the meta recorded.
    let (n, freed) = ws.engine.scratch_remove(Some("//build:gocache"))?;
    assert_eq!(n, 1);
    assert!(freed >= 8 * 1024);
    assert!(ws.engine.scratch_slots()?.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// The remote lineage: publish here, pick it up cold there.
// ---------------------------------------------------------------------------

/// Build an engine rooted at `root` with one read+write remote cache and an
/// explicit lineage policy — a stand-in for one CI runner.
fn remote_engine(
    root: &std::path::Path,
    remote_uri: &str,
    scope: &str,
    fallbacks: &[&str],
) -> Arc<heph::engine::Engine> {
    use heph::engine::{Config, Engine, RemoteCacheDef, ScratchOptions};
    let mut e = Engine::new(Config {
        root: root.to_path_buf(),
        home_dir: std::path::PathBuf::new(),
        remote_caches: vec![RemoteCacheDef {
            name: "shared".to_string(),
            uri: remote_uri.to_string(),
            read: true,
            write: true,
            concurrency: 10,
            endpoint: None,
            region: None,
        }],
        scratch: ScratchOptions {
            scope: scope.to_string(),
            restore_scopes: fallbacks.iter().map(|s| s.to_string()).collect(),
            seed_on_fork: true,
        },
        ..Default::default()
    })
    .expect("engine");
    e.register_provider(|init| {
        Box::new(heph::pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
        ))
    })
    .expect("provider");
    e.register_managed_driver(|_| Box::new(heph::pluginexec::Driver::new_bash()))
        .expect("bash driver");
    Arc::new(e)
}

const REMOTE_DECL: &str = r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C",
       remote = True)"#;

fn remote_target(marker: &str) -> String {
    format!(
        r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = ["cat \"$C/marker\" > $OUT || echo cold > $OUT", "echo {marker} > \"$C/marker\""])"#
    )
}

async fn build(engine: &Arc<heph::engine::Engine>) -> anyhow::Result<String> {
    let addr = heph::htaddr::parse_addr("//app:a")?;
    let opts = ResultOptions::default();
    let r = Arc::clone(engine)
        .result_addr(engine.new_state(), &addr, OutputMatcher::All, &opts)
        .await?;
    let out = common::artifact_string(&r).trim().to_string();
    drop(r);
    Ok(out)
}

/// The CI story end to end: one machine builds and publishes; a second machine,
/// cold in every lineage, picks the snapshot up automatically and sees the first
/// machine's work.
///
/// Publishing is explicit; picking up is not. That asymmetry is the design —
/// a pull is read-only, cheap, and degrades to a cold build, so it is safe to do
/// on its own. A push is none of those.
#[tokio::test]
async fn a_published_snapshot_warms_a_cold_machine() -> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());

    // Machine one: build, then publish.
    let a = tempfile::tempdir()?;
    std::fs::create_dir_all(a.path().join("build"))?;
    std::fs::create_dir_all(a.path().join("app"))?;
    std::fs::write(a.path().join("build").join("BUILD"), REMOTE_DECL)?;
    std::fs::write(
        a.path().join("app").join("BUILD"),
        remote_target("machine-one"),
    )?;

    let e1 = remote_engine(a.path(), &uri, "master", &[]);
    assert_eq!(build(&e1).await?, "cold", "the first machine starts cold");

    let slots = e1.scratch_slots()?;
    assert_eq!(slots.len(), 1);
    let slot = slots[0].slot.clone();
    let dir = heph::engine::scratch_remote::scope_head_dir(&e1.home, &slot, "master");
    let (generation, bytes) = e1
        .scratch_push(&slot, "master", &dir, None, "run-1")
        .await?;
    assert_eq!(generation, 0, "a first publish starts the lineage at 0");
    assert!(bytes > 0);

    // Machine two: a different root, nothing local at all.
    let b = tempfile::tempdir()?;
    std::fs::create_dir_all(b.path().join("build"))?;
    std::fs::create_dir_all(b.path().join("app"))?;
    std::fs::write(b.path().join("build").join("BUILD"), REMOTE_DECL)?;
    std::fs::write(
        b.path().join("app").join("BUILD"),
        remote_target("machine-two"),
    )?;

    let e2 = remote_engine(b.path(), &uri, "master", &[]);
    assert_eq!(
        build(&e2).await?,
        "machine-one",
        "a cold machine must pick up the published snapshot without being asked"
    );
    Ok(())
}

/// Generations advance at publish time and are `parent + 1` within a lineage, so
/// a later publish wins — and republishing identical contents adds nothing.
#[tokio::test]
async fn publishing_advances_the_lineage_and_skips_unchanged_contents() -> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("build"))?;
    std::fs::create_dir_all(ws.path().join("app"))?;
    std::fs::write(ws.path().join("build").join("BUILD"), REMOTE_DECL)?;
    std::fs::write(ws.path().join("app").join("BUILD"), remote_target("one"))?;

    let e = remote_engine(ws.path(), &uri, "master", &[]);
    build(&e).await?;
    let slot = e.scratch_slots()?[0].slot.clone();
    let dir = heph::engine::scratch_remote::scope_head_dir(&e.home, &slot, "master");

    let (g0, _) = e.scratch_push(&slot, "master", &dir, None, "r1").await?;
    assert_eq!(g0, 0);

    // Nothing changed on disk, so there is nothing to say. Publishing anyway
    // would grow the chain with every no-op CI run.
    let parent = heph::engine::scratch_remote::read_local_meta(&e.home, &slot, "master");
    let (g_same, bytes) = e
        .scratch_push(&slot, "master", &dir, parent.as_ref(), "r2")
        .await?;
    assert_eq!(
        (g_same, bytes),
        (0, 0),
        "unchanged contents must not publish"
    );

    // Change the contents, and the lineage advances.
    std::fs::write(dir.join("marker"), b"two\n")?;
    let parent = heph::engine::scratch_remote::read_local_meta(&e.home, &slot, "master");
    let (g1, _) = e
        .scratch_push(&slot, "master", &dir, parent.as_ref(), "r3")
        .await?;
    assert_eq!(g1, 1, "a changed publish is parent + 1");

    let head = e
        .scratch_remote_head(&slot, "master", &[])
        .await
        .expect("a head");
    assert_eq!(head.meta.generation, 1, "the newest generation wins");
    Ok(())
}

/// Reads fall back across branches; writes never do. A PR job picks up `master`'s
/// snapshot and publishes into its own lineage, leaving `master`'s head where it
/// was — the isolation that makes this safe on untrusted CI.
#[tokio::test]
async fn a_branch_reads_from_master_and_publishes_to_itself() -> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());
    let mk = |marker: &str| -> anyhow::Result<tempfile::TempDir> {
        let d = tempfile::tempdir()?;
        std::fs::create_dir_all(d.path().join("build"))?;
        std::fs::create_dir_all(d.path().join("app"))?;
        std::fs::write(d.path().join("build").join("BUILD"), REMOTE_DECL)?;
        std::fs::write(d.path().join("app").join("BUILD"), remote_target(marker))?;
        Ok(d)
    };

    // `master` publishes.
    let m = mk("from-master")?;
    let em = remote_engine(m.path(), &uri, "master", &[]);
    build(&em).await?;
    let slot = em.scratch_slots()?[0].slot.clone();
    let mdir = heph::engine::scratch_remote::scope_head_dir(&em.home, &slot, "master");
    em.scratch_push(&slot, "master", &mdir, None, "master-run")
        .await?;

    // A PR runner, cold, on `pr-1` with `master` as its fallback.
    let p = mk("from-pr")?;
    let ep = remote_engine(p.path(), &uri, "pr-1", &["master"]);
    assert_eq!(
        build(&ep).await?,
        "from-master",
        "a branch must pick up its base's snapshot"
    );

    let pdir = heph::engine::scratch_remote::scope_head_dir(&ep.home, &slot, "pr-1");
    let parent = heph::engine::scratch_remote::read_local_meta(&ep.home, &slot, "pr-1");
    ep.scratch_push(&slot, "pr-1", &pdir, parent.as_ref(), "pr-run")
        .await?;

    // `master`'s lineage is untouched: still generation 0, still its own bytes.
    let master_head = em
        .scratch_remote_head(&slot, "master", &[])
        .await
        .expect("master head");
    assert_eq!(master_head.meta.scope, "master");
    assert_eq!(
        master_head.meta.generation, 0,
        "a PR publish must not advance master's lineage"
    );

    // And the branch has a lineage of its own.
    let pr_head = ep
        .scratch_remote_head(&slot, "pr-1", &[])
        .await
        .expect("pr head");
    assert_eq!(pr_head.meta.scope, "pr-1");
    Ok(())
}

/// `heph tool scratch head` is the "why did my branch start cold?" answer, and the
/// question is only answerable if the trace shows the scopes that held *nothing*.
/// Resolution itself stops at the first hit, so a trace that reported only the
/// winner would be no better than the resolution it explains.
#[tokio::test]
async fn the_resolution_trace_reports_every_candidate_including_the_empty_ones()
-> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());
    let mk = || -> anyhow::Result<tempfile::TempDir> {
        let d = tempfile::tempdir()?;
        std::fs::create_dir_all(d.path().join("build"))?;
        std::fs::create_dir_all(d.path().join("app"))?;
        std::fs::write(d.path().join("build").join("BUILD"), REMOTE_DECL)?;
        std::fs::write(d.path().join("app").join("BUILD"), remote_target("x"))?;
        Ok(d)
    };

    // Only `master` ever publishes. `release` never does.
    let m = mk()?;
    let em = remote_engine(m.path(), &uri, "master", &[]);
    build(&em).await?;
    let slot = em.scratch_slots()?[0].slot.clone();
    let mdir = heph::engine::scratch_remote::scope_head_dir(&em.home, &slot, "master");
    em.scratch_push(&slot, "master", &mdir, None, "ci-42")
        .await?;

    // Asked from `pr-1`, falling back to `release` then `master`.
    let trace = em
        .scratch_remote_trace(
            &slot,
            "pr-1",
            &["release".to_string(), "master".to_string()],
        )
        .await;

    assert_eq!(
        trace.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        ["pr-1", "release", "master"],
        "the trace must be the consult order, own scope first"
    );
    assert!(
        trace[0].1.is_none() && trace[1].1.is_none(),
        "a scope nobody published to holds nothing: {trace:?}"
    );
    let winner = trace[2].1.as_ref().expect("master holds the snapshot");
    assert_eq!(winner.meta.scope, "master");
    // The producer is the field that turns "it came from master" into "it came
    // from *that* run", which is the whole point of recording it.
    assert_eq!(winner.meta.producer, "ci-42");

    // And the trace agrees with the resolution it explains — the first scope with
    // anything is what a cold build here would restore.
    let resolved = em
        .scratch_remote_head(
            &slot,
            "pr-1",
            &["release".to_string(), "master".to_string()],
        )
        .await
        .expect("resolves to master");
    assert_eq!(resolved.stem, winner.stem);
    Ok(())
}

/// A remote that is unreachable is a cold build, never a failed one — the scratch
/// contract in its most load-bearing form.
#[tokio::test]
async fn an_unreachable_remote_degrades_to_a_cold_build() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("build"))?;
    std::fs::create_dir_all(ws.path().join("app"))?;
    std::fs::write(ws.path().join("build").join("BUILD"), REMOTE_DECL)?;
    std::fs::write(ws.path().join("app").join("BUILD"), remote_target("x"))?;

    // A path that does not exist and cannot be created.
    let e = remote_engine(
        ws.path(),
        "file:///nonexistent/heph-scratch-test",
        "master",
        &[],
    );
    assert_eq!(
        build(&e).await?,
        "cold",
        "a dead remote must not fail a build"
    );
    Ok(())
}

/// What a broad glob beside a mounted scratch actually does.
///
/// It used to be rejected at *parse* time, on the reasoning that the glob would
/// sweep the cache into the artifact. That reasoning was wrong: collection uses
/// `symlink_metadata` and takes `is_file() || is_symlink()`, and `walkdir` does
/// not follow symlinks — so a glob reaching a mount collects the *symlink*, never
/// what is behind it. The parse-time guard was also redundant, because the packer
/// already refuses an absolute symlink, and says so precisely.
///
/// So this is the behaviour today: the build runs, and packing fails naming the
/// mount. **Still not right** — heph created that symlink, the author did not, so
/// collection ought to skip it rather than hand the author an error about it.
/// Fixing that means threading the mount paths into `collect_outputs`; until
/// then this test pins what actually happens rather than what should.
///
/// The overlap that is genuinely invalid is a mount landing on a materialized
/// input — that destroys a real file, and is covered by
/// `a_scratch_cannot_mount_over_a_materialized_input`.
#[tokio::test]
async fn a_broad_glob_beside_a_mount_fails_in_the_packer_not_the_parser() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "**/*", scratch = ["//build:c"],
       run = ["echo hi > o.txt", "echo cached > \"$C/entry\""])"#,
    );

    let err = expect_err(
        ws.run("//app:a").await,
        "packing must refuse the mount symlink",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("absolute symlink not allowed"),
        "the packer's own check is what rejects it now: {msg}"
    );
    // And it names the mount, which is the part a person needs.
    assert!(msg.contains(".cache/x"), "must name the mount: {msg}");
    Ok(())
}

/// `pull` must work on a machine that has never built — warming exactly that
/// machine is what it is for. Selecting from the local store instead of the graph
/// made it a no-op in its only real use case.
///
/// And a pull-warmed slot must still describe itself, or the store stops being
/// listable and removable the moment it is populated any way but by building.
#[tokio::test]
async fn a_cold_machine_can_pull_what_it_has_never_built() -> anyhow::Result<()> {
    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());

    // Machine one publishes.
    let a = tempfile::tempdir()?;
    std::fs::create_dir_all(a.path().join("build"))?;
    std::fs::create_dir_all(a.path().join("app"))?;
    std::fs::write(a.path().join("build").join("BUILD"), REMOTE_DECL)?;
    std::fs::write(
        a.path().join("app").join("BUILD"),
        remote_target("published"),
    )?;
    let e1 = remote_engine(a.path(), &uri, "master", &[]);
    build(&e1).await?;
    let slot = e1.scratch_slots()?[0].slot.clone();
    let dir = heph::engine::scratch_remote::scope_head_dir(&e1.home, &slot, "master");
    e1.scratch_push(&slot, "master", &dir, None, "run-1")
        .await?;

    // Machine two has built nothing at all, so it has no slots to enumerate.
    let b = tempfile::tempdir()?;
    std::fs::create_dir_all(b.path().join("build"))?;
    std::fs::write(b.path().join("build").join("BUILD"), REMOTE_DECL)?;
    let e2 = remote_engine(b.path(), &uri, "master", &[]);
    assert!(
        e2.scratch_slots()?.is_empty(),
        "precondition: the second machine is genuinely cold"
    );

    // The head is discoverable without any local state — which is what makes a
    // graph-driven `pull` possible.
    let head = e2
        .scratch_remote_head(&slot, "master", &[])
        .await
        .expect("a cold machine must still find the published head");
    let dir2 = heph::engine::scratch_remote::scope_head_dir(&e2.home, &slot, "master");
    let bytes = e2.scratch_pull(&head, &dir2).await?;
    assert!(bytes > 0);
    assert!(dir2.join("marker").exists(), "the payload must have landed");
    Ok(())
}
