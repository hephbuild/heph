//! The BUILD-file LSP's listings through the real engine
//! ([`LspEngine::list`]), with the buildfile provider and the Go plugin
//! registered as the shipped binary has them.
//!
//! Each call is its own root request, so its own cache scope: these tests pin
//! that a listing sees the workspace as it is now, and the `#[ignore]`d
//! instrument at the bottom measures what that costs per completion.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use heph::engine::PluginParts;
use hplugin::lsp::{Listed, Listing, LspEngine as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A workspace with the buildfile provider, the `bash` driver and the `go`
/// plugin (its provider and its `heph.go.*` functions).
fn workspace() -> anyhow::Result<htestkit::Workspace> {
    htestkit::WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .with_plugin("go", |init| {
            let go =
                hplugin_go::plugingo::Provider::new(init.root.to_path_buf(), init.runtime.clone())?;
            Ok(PluginParts::default()
                .with_functions(go.functions())
                .with_provider(Box::new(go)))
        })
        .build()
}

async fn list(ws: &htestkit::Workspace, what: Listing) -> anyhow::Result<Listed> {
    Arc::clone(&ws.engine).list(what).await
}

fn names(listed: &Listed) -> Vec<&str> {
    listed.entries.iter().map(|e| e.name.as_str()).collect()
}

fn packages(under: &str) -> Listing {
    Listing::Packages {
        under: under.to_string(),
    }
}

fn targets(package: &str) -> Listing {
    Listing::Targets {
        package: package.to_string(),
    }
}

/// `//@heph/` completes to the Go plugin's gocache package, and
/// `//@heph/go/gocache:` to its module cache: the one synthetic Go target a
/// BUILD file names bare (`scratch = ["//@heph/go/gocache:modcache"]`).
#[tokio::test]
async fn completion_lists_the_go_gocache_package_and_modcache() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file(
        "app",
        r#"target(name = "t", driver = "bash", run = "true")"#,
    );

    // `//@heph/` lists the packages under `@heph`; `//@he` lists them all and
    // filters on the partial segment.
    for under in ["@heph", ""] {
        let listed = list(&ws, packages(under)).await?;
        let gocache = listed
            .entries
            .iter()
            .find(|e| e.name == "@heph/go/gocache")
            .unwrap_or_else(|| panic!("under {under:?}: {:?}", names(&listed)));
        assert_eq!(gocache.provider, "go", "under {under:?}");
        assert!(listed.skipped.is_empty(), "{:?}", listed.skipped);
    }

    let listed = list(&ws, targets("@heph/go/gocache")).await?;
    assert_eq!(names(&listed), ["modcache"], "{:?}", listed.skipped);
    assert_eq!(listed.entries[0].provider, "go");
    Ok(())
}

/// The reason each listing is its own scope: an editor session holds one
/// engine for hours, and a BUILD file edited on disk must show on the next
/// completion. A target added and a target removed, a package created, and a
/// broken BUILD file fixed, each seen by the very next call.
#[tokio::test]
async fn next_listing_sees_a_build_file_edited_on_disk() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        r#"target(name = "old", driver = "bash", run = "true")
target(name = "kept", driver = "bash", run = "true")"#,
    );

    let before = list(&ws, targets("p")).await?;
    assert_eq!(names(&before), ["kept", "old"], "{:?}", before.skipped);
    let pkgs = list(&ws, packages("")).await?;
    assert!(!names(&pkgs).contains(&"q"), "{:?}", names(&pkgs));

    ws.write_build_file(
        "p",
        r#"target(name = "new", driver = "bash", run = "true")
target(name = "kept", driver = "bash", run = "true")"#,
    );
    ws.write_build_file("q", "target(name = 'broken', driver = 'bash', run = )");

    let after = list(&ws, targets("p")).await?;
    assert_eq!(names(&after), ["kept", "new"], "{:?}", after.skipped);
    assert_ne!(after.scope, before.scope, "each listing is its own scope");
    let pkgs = list(&ws, packages("")).await?;
    assert!(names(&pkgs).contains(&"q"), "{:?}", names(&pkgs));

    // A broken package costs only itself, and is reported.
    let broken = list(&ws, targets("q")).await?;
    assert!(broken.entries.is_empty(), "{:?}", names(&broken));
    assert!(!broken.skipped.is_empty(), "the parse error is reported");

    ws.write_build_file(
        "q",
        r#"target(name = "fixed", driver = "bash", run = "true")"#,
    );
    let fixed = list(&ws, targets("q")).await?;
    assert_eq!(names(&fixed), ["fixed"], "{:?}", fixed.skipped);
    assert!(fixed.skipped.is_empty(), "{:?}", fixed.skipped);
    Ok(())
}

/// A generated workspace of `D * E * P` BUILD packages, `d<i>/e<j>/p<k>`, each
/// with [`TARGETS_PER_PKG`] targets.
const D: usize = 10;
const E: usize = 10;
const P: usize = 20;
const TARGETS_PER_PKG: usize = 4;
const WARM_REPEATS: usize = 20;

/// The cost of a scope per completion, on a 2000-package workspace: the
/// first call (cold, the OS page cache aside) and the median of
/// [`WARM_REPEATS`] more. Each call opens a fresh scope, so warm repeats
/// re-walk and re-evaluate exactly as a completion in a long editor session
/// does.
///
/// An instrument, not a test: it asserts nothing and prints a table.
///
/// ```text
/// cargo test --release -p e2e --test e2e lsp_listing:: -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "instrument: prints timings, asserts nothing"]
async fn scope_per_completion_cost() -> anyhow::Result<()> {
    let ws = workspace()?;
    let mut build = String::new();
    for t in 0..TARGETS_PER_PKG {
        build.push_str(&format!(
            "target(name = \"t{t}\", driver = \"bash\", run = \"echo {t} > $OUT\", out = \"o{t}\")\n"
        ));
    }
    for d in 0..D {
        for e in 0..E {
            for p in 0..P {
                ws.write_build_file(&format!("d{d}/e{e}/p{p}"), &build);
            }
        }
    }
    let total = D * E * P;

    let calls = [
        ("Packages under \"\"", packages("")),
        ("Packages under \"d3/e4\"", packages("d3/e4")),
        ("Targets in d3/e4/p5", targets("d3/e4/p5")),
    ];
    println!("\n{total} packages, {TARGETS_PER_PKG} targets each");
    println!("| call | entries | cold | warm median | warm min | warm max |");
    println!("|---|---|---|---|---|---|");
    for (label, what) in calls {
        let start = Instant::now();
        let listed = list(&ws, what.clone()).await?;
        let cold = start.elapsed();
        let entries = listed.entries.len();
        assert!(listed.skipped.is_empty(), "{:?}", listed.skipped);
        let mut warm: Vec<Duration> = Vec::with_capacity(WARM_REPEATS);
        for _ in 0..WARM_REPEATS {
            let start = Instant::now();
            let listed = list(&ws, what.clone()).await?;
            warm.push(start.elapsed());
            assert_eq!(listed.entries.len(), entries);
        }
        warm.sort_unstable();
        println!(
            "| {label} | {entries} | {cold:.1?} | {:.1?} | {:.1?} | {:.1?} |",
            warm[WARM_REPEATS / 2],
            warm[0],
            warm[WARM_REPEATS - 1],
        );
    }
    Ok(())
}
