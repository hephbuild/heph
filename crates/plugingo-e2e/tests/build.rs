mod common;

use anyhow::Context as _;
use common::{artifact_paths, fixture, make_workspace, make_workspace_host, require_go};

#[tokio::test]
async fn test_simple_lib_build_lib() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("simple_lib")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build_lib should produce at least one artifact"
    );
    Ok(())
}

/// End-to-end proof of the integrated `go_compile` embed path: the driver reads
/// `_golist`'s package.bin for the `//go:embed` patterns, resolves the embedcfg
/// in-process, stages the embed file, and compiles with `-embedcfg`. A green
/// build means the whole embed-in-compile pipeline works (no separate
/// `go_embed` target).
#[tokio::test]
async fn test_embed_build_lib_compiles_with_embedcfg() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "embedding build_lib should compile and produce an archive"
    );
    Ok(())
}

/// Embed a file living in a SUBDIRECTORY (`//go:embed resources/script.sh`).
/// Regression: the non-go `**/*` source glob must stage subdir files into the
/// `_golist` sandbox so `go list` resolves the pattern into `EmbedFiles`; a
/// failure surfaces as `compute embedcfg: //go:embed pattern(s) matched no files`.
#[tokio::test]
async fn test_embed_subdir_build_lib_compiles_with_embedcfg() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_subdir")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "subdir-embedding build_lib should compile and produce an archive"
    );
    Ok(())
}

/// Embed a subdir file from a NON-root package (`//app:build_lib`, embed
/// `resources/script.sh`). Exercises the `{pkg}/**/*` glob for a nested package.
#[tokio::test]
async fn test_embed_nested_pkg_build_lib_compiles_with_embedcfg() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_nested")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//app:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "nested-package subdir-embedding build_lib should compile"
    );
    Ok(())
}

/// Embed a subdir file from a package whose embedding .go file is behind a
/// `//go:build linux` constraint, built for goos=linux. Mirrors infhostd:
/// the pattern reaches EmbedPatterns but the file must still resolve into
/// EmbedFiles for the linux build context.
#[tokio::test]
async fn test_embed_buildtagged_file_compiles_with_embedcfg() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_buildtag")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//app:build_lib@v=linux_amd64").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build-tagged embedding build_lib should compile for goos=linux"
    );
    Ok(())
}

/// Embed via a `go_embed_src`-labeled `group` target wrapping `glob("resources/*")`
/// (the decoupled embed lane). golist excludes go_embed_src, so go list reports
/// empty EmbedFiles by design and the compile must resolve the pattern from the
/// staged go_embed_src files. Repro for the infhostd failure.
#[tokio::test]
async fn test_embed_group_go_embed_src_compiles() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_group")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "go_embed_src group embedding build_lib should compile"
    );
    Ok(())
}

/// Embed a GENERATED file via the go_embed_src lane: a codegen target produces
/// resources/gen.sh (labelled go_embed_src), embedded by //go:embed. golist
/// excludes go_embed_src, so the file is never on disk for go list — the compile
/// must stage+resolve it through the embed_src lane + selector. The decoupling's
/// real path; closest to the infhostd report.
#[tokio::test]
async fn test_embed_generated_go_embed_src_compiles() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_gen")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "generated go_embed_src embedding build_lib should compile"
    );
    Ok(())
}

/// `build_test_lib` (TestEmbed variant) for a build-tagged on-disk embed,
/// goos=linux. The test compile unions embed+test_embed patterns/files; this
/// exercises the path the infhostd `build_test_lib` failure hit.
#[tokio::test]
async fn test_embed_buildtagged_build_test_lib_compiles() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_buildtag")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//app:build_test_lib@v=linux_amd64").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build-tagged build_test_lib should compile for goos=linux"
    );
    Ok(())
}

#[tokio::test]
async fn test_with_dep_cmd_build() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_dep")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//cmd:build@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "cmd build should produce at least one artifact"
    );
    Ok(())
}

/// End-to-end proof of the `go_compile` **assembly** pipeline (symabis →
/// compile → per-`.s` asm → `pack r`). First-party packages never carry `.s`
/// files in this provider, so asm is only reachable through a thirdparty module:
/// `klauspost/cpuid/v2` ships per-arch assembly. Building its `build_lib` runs
/// the whole asm path. The module download needs the network, so a download
/// failure (offline CI) skips; any *other* failure (i.e. the asm compile itself)
/// fails loudly so a real regression isn't hidden.
#[tokio::test]
async fn test_thirdparty_asm_build_lib_compiles() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("thirdparty_asm")?;
    let ws = make_workspace(dir)?;
    let lib = "//@heph/go/thirdparty/github.com/klauspost/cpuid/v2@v2.2.5:build_lib@v=host";
    match ws.run(lib).await {
        Ok(result) => {
            assert!(
                !artifact_paths(&result).is_empty(),
                "cpuid build_lib (asm) should compile and produce an archive"
            );
        }
        Err(e) => {
            let msg = format!("{e:?}").to_lowercase();
            let networky = msg.contains("download")
                || msg.contains("go mod")
                || msg.contains("dial")
                || msg.contains("lookup")
                || msg.contains("connection")
                || msg.contains("proxy")
                || msg.contains("timeout");
            if networky {
                eprintln!("skipping thirdparty asm e2e (module download unavailable): {e:?}");
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Same build, but with `gotool = "host"`: the provider uses the host `go`
/// (resolved from PATH / `go env GOROOT` in-sandbox) instead of a hermetic SDK.
#[tokio::test]
async fn test_with_dep_cmd_build_host_toolchain() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_dep")?;
    let ws = make_workspace_host(dir)?;
    let result = ws.run("//cmd:build@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "host-toolchain cmd build should produce at least one artifact"
    );
    Ok(())
}

/// Regression: a package mixing a go_src static `//go:embed` (resources/x.sh on
/// disk, no target) with a go_embed_src embed (ui_dist, decoupled out of golist).
/// `go list` resolves embeds atomically, so the unresolved go_embed_src patterns
/// must NOT poison resolution of the co-located static embed.
#[tokio::test]
async fn test_embed_mixed_go_src_and_go_embed_src_compiles() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_mixed")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "mixed go_src + go_embed_src embedding build_lib should compile"
    );
    Ok(())
}

#[tokio::test]
async fn test_embed_mixed_build_testmain_lib() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_embed_mixed")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_testmain_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "testmain_lib should compile"
    );
    Ok(())
}

/// The embedded asset is produced by a codegen target that itself runs a Go tool
/// built in-tree — so the asset's target transitively depends on that package's
/// `_golist`, while `_golist`'s own source set is a *query* over the codegen root.
/// A query candidate that cycles is skipped, and which side of a cycle is detected
/// depends on scheduling — so this must not resolve differently run to run.
///
/// Run twice: the failure this guards ("//go:embed pattern(s) matched no files")
/// is scheduling-dependent, so a single green run proves little.
#[tokio::test]
async fn test_embed_generated_by_in_tree_tool_resolves_deterministically() -> anyhow::Result<()> {
    require_go!();
    // Host go: a hermetic SDK would re-download per attempt.
    for attempt in 0..2 {
        let ws = make_workspace_host(fixture("embed_gen_tool")?)?;
        let result = ws
            .run("//app:build_lib@v=host")
            .await
            .with_context(|| format!("attempt {attempt}"))?;
        assert!(
            !artifact_paths(&result).is_empty(),
            "attempt {attempt}: build_lib must compile"
        );
    }
    Ok(())
}

/// The embedded asset is generated by a target in a **sub**-package of the Go
/// package that embeds it (`app/openapi` generating into `app`'s tree) — the shape
/// a real repo takes when an OpenAPI spec is bundled next to the code embedding it,
/// with the codegen root declared in that sub-package.
///
/// `tree_output(app)` matches it (its output lands under `app/`), but the `go_src`
/// query's *scope* term has to reach it too. Nothing else can: the fs glob skips
/// codegen-stamped files, so if the query misses it, the file reaches `go list`
/// only when the on-disk copy happens to be unstamped — a coin flip, which is how
/// this surfaced in the wild (~1 run in 3 failing with "matched no files").
#[tokio::test]
async fn test_embed_generated_in_subpackage_of_the_embedder() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace_host(fixture("embed_gen_subpkg")?)?;
    let result = ws.run("//app:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build_lib embedding a sub-package's generated asset must compile"
    );
    Ok(())
}
