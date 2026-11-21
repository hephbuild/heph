//! The BUILD-file LSP's listings through the real engine
//! ([`LspEngine::list`]), with the buildfile provider and the Go and OCI
//! plugins' providers registered as the shipped binary has them.
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

/// A workspace with the buildfile provider, the `bash` driver, the `go`
/// plugin (its provider and its `heph.go.*` functions) and the `oci`
/// plugin's provider (`//@heph/oci:platform`, `//@heph/oci:blobs`).
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
        .with_plugin("oci", |init| {
            Ok(PluginParts::default().with_provider(Box::new(
                hplugin_oci::pluginoci::platform::Provider::new(&init.name),
            )))
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
        assert_eq!(&*gocache.provider, "go", "under {under:?}");
        assert!(listed.skipped.is_empty(), "{:?}", listed.skipped);
    }

    let listed = list(&ws, targets("@heph/go/gocache")).await?;
    assert_eq!(names(&listed), ["modcache"], "{:?}", listed.skipped);
    assert_eq!(&*listed.entries[0].provider, "go");
    Ok(())
}

/// `//@heph/oci:` completes to the OCI plugin's blob store, the synthetic
/// target a BUILD file names bare (`scratch = ["//@heph/oci:blobs"]`), both
/// from the engine and through the site's listing choice.
#[tokio::test(flavor = "multi_thread")]
async fn completion_lists_the_oci_blobs_target() -> anyhow::Result<()> {
    let ws = workspace()?;
    let listed = list(&ws, packages("@heph")).await?;
    let oci = listed
        .entries
        .iter()
        .find(|e| e.name == "@heph/oci")
        .unwrap_or_else(|| panic!("{:?}", names(&listed)));
    assert_eq!(&*oci.provider, "oci");

    let listed = list(&ws, targets("@heph/oci")).await?;
    let blobs = listed
        .entries
        .iter()
        .find(|e| e.name == "blobs")
        .unwrap_or_else(|| panic!("{:?} {:?}", names(&listed), listed.skipped));
    assert_eq!(&*blobs.provider, "oci");

    let text = r#"target(name = "t", driver = "bash", run = "true", scratch = ["//@heph/oci:bl"])"#;
    let out = explain(&ws, text, ":bl", true, Some(Arc::clone(&ws.engine))).await?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(
        v["listing"]["candidates"],
        serde_json::json!([{ "name": "blobs", "provider": "oci" }]),
        "{out}"
    );
    Ok(())
}

/// The package walk is per scope: a package created on disk shows on the
/// very next listing, and one whose BUILD file is removed is gone from it.
#[tokio::test]
async fn next_listing_sees_a_package_created_or_removed_on_disk() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file("p", r#"target(name = "t", driver = "bash", run = "true")"#);

    let before = list(&ws, packages("")).await?;
    assert!(names(&before).contains(&"p"), "{:?}", names(&before));
    assert!(!names(&before).contains(&"q"), "{:?}", names(&before));

    ws.write_build_file("q", r#"target(name = "t", driver = "bash", run = "true")"#);
    let created = list(&ws, packages("")).await?;
    assert!(names(&created).contains(&"q"), "{:?}", names(&created));
    assert_ne!(created.scope, before.scope, "each listing is its own scope");
    let in_q = list(&ws, targets("q")).await?;
    assert_eq!(names(&in_q), ["t"], "{:?}", in_q.skipped);

    std::fs::remove_file(ws.dir.path().join("q/BUILD"))?;
    let removed = list(&ws, packages("")).await?;
    assert!(!names(&removed).contains(&"q"), "{:?}", names(&removed));
    assert!(names(&removed).contains(&"p"), "{:?}", names(&removed));
    Ok(())
}

/// A package's evaluation is per scope: a target renamed, then the BUILD file
/// broken, then fixed, each seen by the very next listing of that package.
#[tokio::test]
async fn next_listing_sees_a_build_file_edited_broken_and_fixed() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        r#"target(name = "old", driver = "bash", run = "true")
target(name = "kept", driver = "bash", run = "true")"#,
    );
    let before = list(&ws, targets("p")).await?;
    assert_eq!(names(&before), ["kept", "old"], "{:?}", before.skipped);

    ws.write_build_file(
        "p",
        r#"target(name = "new", driver = "bash", run = "true")
target(name = "kept", driver = "bash", run = "true")"#,
    );
    let edited = list(&ws, targets("p")).await?;
    assert_eq!(names(&edited), ["kept", "new"], "{:?}", edited.skipped);
    assert_ne!(edited.scope, before.scope, "each listing is its own scope");

    // A broken package costs only itself, and is reported.
    ws.write_build_file("p", "target(name = 'broken', driver = 'bash', run = )");
    let broken = list(&ws, targets("p")).await?;
    assert!(broken.entries.is_empty(), "{:?}", names(&broken));
    assert!(!broken.skipped.is_empty(), "the parse error is reported");

    ws.write_build_file(
        "p",
        r#"target(name = "fixed", driver = "bash", run = "true")"#,
    );
    let fixed = list(&ws, targets("p")).await?;
    assert_eq!(names(&fixed), ["fixed"], "{:?}", fixed.skipped);
    assert!(fixed.skipped.is_empty(), "{:?}", fixed.skipped);
    Ok(())
}

/// `Discovery::KeepGoing`: a sibling package whose BUILD file does not
/// evaluate costs only its own entries. The healthy package is still listed,
/// and so are its targets.
#[tokio::test]
async fn a_broken_sibling_does_not_hide_the_healthy_package() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file("ok", r#"target(name = "t", driver = "bash", run = "true")"#);
    ws.write_build_file("bad", "target(name = 'x', driver = 'bash', run = )");
    ws.write_build_file("worse", r#"load("//nowhere:defs.star", "nope")"#);
    ws.write_build_file(
        "bad/sub",
        r#"target(name = "s", driver = "bash", run = "true")"#,
    );

    let pkgs = list(&ws, packages("")).await?;
    for healthy in ["ok", "bad/sub"] {
        assert!(
            names(&pkgs).contains(&healthy),
            "{healthy}: {:?} {:?}",
            names(&pkgs),
            pkgs.skipped
        );
    }
    let under_bad = list(&ws, packages("bad")).await?;
    assert!(
        names(&under_bad).contains(&"bad/sub"),
        "{:?}",
        names(&under_bad)
    );

    // (`bad/sub`'s targets are not isolated from `bad`: the buildfile
    // provider's probe of a package evaluates its ancestors' BUILD files.)
    let in_ok = list(&ws, targets("ok")).await?;
    assert_eq!(names(&in_ok), ["t"], "{:?}", in_ok.skipped);
    assert!(in_ok.skipped.is_empty(), "{:?}", in_ok.skipped);
    // Each broken package costs its own listing, reported, not an error.
    for pkg in ["bad", "worse"] {
        let listed = list(&ws, targets(pkg)).await?;
        assert!(listed.entries.is_empty(), "{pkg}: {:?}", names(&listed));
        assert!(!listed.skipped.is_empty(), "{pkg}: the failure is reported");
    }
    Ok(())
}

/// A package nobody provides lists no targets, and is not an error: the
/// editor is mid-way through typing its name.
#[tokio::test]
async fn targets_of_a_nonexistent_package_are_empty() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file("p", r#"target(name = "t", driver = "bash", run = "true")"#);
    let listed = list(&ws, targets("no/such/pkg")).await?;
    assert!(listed.entries.is_empty(), "{:?}", names(&listed));
    assert!(listed.skipped.is_empty(), "{:?}", listed.skipped);
    Ok(())
}

/// A typed path that would leave the workspace (`//../..`, `///etc`) lists
/// nothing, with every provider registered: the completion's listing choice
/// refuses it, so no provider is asked, and `--explain` says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_traversal_prefix_lists_nothing_outside_the_workspace() -> anyhow::Result<()> {
    let ws = workspace()?;
    // The completion refuses these before it asks; `Engine::list` refuses
    // them again (its own unit test), so the Go provider never walks them.
    for (needle, reason) in [
        ("//../..", "dot_segment"),
        ("//../", "dot_segment"),
        ("///etc", "absolute"),
        ("//../x:", "dot_segment"),
    ] {
        let text =
            format!(r#"target(name = "t", driver = "bash", run = "true", deps = ["{needle}"])"#);
        let out = explain(&ws, &text, needle, true, Some(Arc::clone(&ws.engine))).await?;
        let v: serde_json::Value = serde_json::from_str(&out)?;
        assert_eq!(v["listing"]["error"]["stage"], "path", "{needle}: {out}");
        assert_eq!(v["listing"]["error"]["reason"], reason, "{needle}: {out}");
        assert!(v["listing"].get("candidates").is_none(), "{needle}: {out}");
    }
    // `//..` is a partial first segment, not a traversal: it lists the
    // workspace's packages and keeps none of them.
    let text = r#"target(name = "t", driver = "bash", run = "true", deps = ["//.."])"#;
    let out = explain(&ws, text, "//..", true, Some(Arc::clone(&ws.engine))).await?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v["listing"]["query"]["packages_under"], "", "{out}");
    assert_eq!(v["listing"]["candidates"], serde_json::json!([]), "{out}");
    Ok(())
}

/// `heph tool build-lsp --explain FILE:LINE:COL` at `needle`'s end in `app`'s
/// BUILD file, through the engine the closure hands it (`None`: no engine,
/// and calling for one fails the explain).
async fn explain(
    ws: &htestkit::Workspace,
    text: &str,
    needle: &str,
    json: bool,
    engine: Option<Arc<heph::engine::Engine>>,
) -> anyhow::Result<String> {
    ws.write_build_file("app", text);
    let file = ws.dir.path().join("app/BUILD");
    let offset = text
        .find(needle)
        .map(|i| i + needle.len())
        .unwrap_or_default();
    let (line, col) = text
        .get(..offset)
        .map(|before| {
            let line = before.matches('\n').count() + 1;
            let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            (line, col)
        })
        .unwrap_or((1, 1));
    let target = format!("{}:{line}:{col}", file.display());
    // `--explain` blocks on the listing, as the command's main thread does;
    // a blocking thread keeps this runtime's workers free to run it.
    tokio::task::spawn_blocking(move || {
        heph::pluginbuildfile::lsp_explain(&target, json, || match engine {
            Some(engine) => Ok(engine as Arc<dyn hplugin::lsp::LspEngine>),
            None => anyhow::bail!("this site must not build an engine"),
        })
    })
    .await?
}

/// `--explain` at an address shows the listing its completion runs: the
/// scope, each candidate with the provider it came from, and how each
/// provider answered the end of that scope. `:name` lists the document's own
/// package; a site that lists nothing never builds an engine.
#[tokio::test(flavor = "multi_thread")]
async fn explain_names_the_scope_and_each_candidates_provider() -> anyhow::Result<()> {
    let ws = workspace()?;
    let text = r#"target(name = "t", driver = "bash", run = "true", deps = ["//@heph/go/gocache:mod", ":t"])"#;

    let out = explain(&ws, text, ":mod", true, Some(Arc::clone(&ws.engine))).await?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    let listing = &v["listing"];
    assert_eq!(listing["query"]["targets_in"], "@heph/go/gocache", "{out}");
    assert!(listing["scope"].as_u64().is_some_and(|s| s > 0), "{out}");
    assert_eq!(
        listing["candidates"],
        serde_json::json!([{ "name": "modcache", "provider": "go" }]),
        "{out}"
    );
    let acks = listing["scope_ended"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        acks.iter()
            .any(|a| a["provider"] == "go" && a["outcome"].is_string()),
        "every provider told the scope ended answers: {out}"
    );

    // The text form, and `:t` resolved against the document's package.
    let out = explain(&ws, text, "\":t", false, Some(Arc::clone(&ws.engine))).await?;
    assert!(out.contains("site: address"), "{out}");
    assert!(out.contains("listing: targets in //app"), "{out}");
    assert!(out.contains("\n  scope: "), "{out}");
    assert!(out.contains("candidate: \"t\" from buildfile"), "{out}");
    assert!(out.contains("scope ended: "), "{out}");

    // Not an address: classified without an engine.
    let out = explain(&ws, text, "driver = \"ba", false, None).await?;
    assert!(!out.contains("listing"), "{out}");
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
