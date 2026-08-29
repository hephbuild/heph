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
    platform = "os_arch",
    version = "go1.23",
    remote = True,
)
"#,
    );

    let spec = ws.get_spec("//build:gocache").await?;
    assert_eq!(spec.driver, "scratch");
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

/// `path` is what a consumer mounts, so a missing one is not a defaultable
/// omission — it is an incomplete declaration, and the error must land at parse
/// time in the package that wrote it.
#[tokio::test]
async fn a_declaration_without_a_path_fails_at_parse() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", version = "1")"#,
    );

    let err = expect_err(
        ws.run("//build:c").await,
        "a scratch without `path` must not resolve",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("path"), "error must name the field: {msg}");
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

/// The central property (`docs/SCRATCH.md` §6.3): **nothing about a scratch
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
        enabled: true,
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
        enabled: true,
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
async fn scratch_off_runs_as_though_nothing_were_declared() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = ".cache/x", env = "C")"#,
    );
    // Reports whether the variable and the mount are there at all.
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", scratch = ["//build:c"],
       run = ["if [ -n \"${C:-}\" ] && [ -d .cache/x ]; then echo mounted > $OUT; else echo absent > $OUT; fi"])"#,
    );

    let on = |enabled: bool| heph::engine::ScratchOptions {
        enabled,
        ..Default::default()
    };
    // `force`, because that is what the CLI does for `--scratch=off` and why it
    // has to: scratch never reaches `hashin`, so without it the second run is a
    // cache hit that replays the result built *with* a warm cache — the audit
    // would pass by reading exactly the answer it is meant to re-derive.
    let run = async |engine: &Arc<heph::engine::Engine>, force: bool| -> anyhow::Result<String> {
        let addr = heph::htaddr::parse_addr("//app:a")?;
        let opts = ResultOptions {
            force,
            ..Default::default()
        };
        let r = Arc::clone(engine)
            .result_addr(engine.new_state(), &addr, OutputMatcher::All, &opts)
            .await?;
        let out = common::artifact_string(&r).trim().to_string();
        drop(r);
        Ok(out)
    };

    assert_eq!(run(&ws.reopen_scoped(on(true))?, false).await?, "mounted");

    // The trap this test originally fell into, asserted *before* the forced run
    // so nobody "simplifies" the implication away later: scratch never reaches
    // `hashin`, so an unforced off-run is a plain cache hit and replays the
    // result built with a warm cache. The audit would pass by reading exactly the
    // answer it is meant to re-derive.
    assert_eq!(
        run(&ws.reopen_scoped(on(false))?, false).await?,
        "mounted",
        "a cached result is served regardless of scratch — which is why \
         `--scratch=off` must imply a rebuild"
    );

    // With the rebuild the CLI forces, the cache is genuinely absent — not merely
    // mounted-and-empty.
    assert_eq!(
        run(&ws.reopen_scoped(on(false))?, true).await?,
        "absent",
        "--scratch=off must leave the cache genuinely absent, not merely empty"
    );
    Ok(())
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
