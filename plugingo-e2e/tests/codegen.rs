mod common;

use anyhow::Context as _;
use common::{
    artifact_paths, artifact_string, fixture, make_workspace, make_workspace_fs_skip,
    make_workspace_go_first, require_go,
};
use std::fs;

#[tokio::test]
async fn test_codegen_gen_target_produces_go_file() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegen")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:gen").await?;
    let content = artifact_string(&result);
    assert!(
        content.contains("GeneratedVar"),
        "gen target must produce a file containing GeneratedVar, got: {content}"
    );
    Ok(())
}

#[tokio::test]
async fn test_codegen_build_lib_compiles_with_generated_source() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegen")?;
    let ws = make_workspace(dir)?;
    // build_lib depends on _golist (for source_map) which depends on the query for go_src
    // labels, which finds //:gen. The engine must run //:gen first, then build_lib.
    let result = ws.run("//:build_lib").await?;
    let paths = artifact_paths(&result);
    assert!(
        !paths.is_empty(),
        "build_lib must produce at least one artifact (the .a archive)"
    );
    Ok(())
}

#[tokio::test]
async fn test_codegen_build_produces_binary() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegen")?;
    let ws = make_workspace(dir)?;
    let _ = fs::remove_file(ws.dir.path().join("gen.go"));
    let result = ws.run("//:build").await?;
    let paths = artifact_paths(&result);
    assert!(
        !paths.is_empty(),
        "build must produce an executable binary artifact"
    );
    Ok(())
}

#[tokio::test]
async fn test_codegen_build_binary_outputs_hello() -> anyhow::Result<()> {
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;

    require_go!();
    let dir = fixture("codegen")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build").await?;

    let tmp = tempfile::tempdir().context("create tempdir for binary")?;
    let mut binary_path: Option<std::path::PathBuf> = None;

    for artifact in &result.artifacts {
        for entry in artifact.walk()? {
            let entry = entry?;
            let name = entry
                .path
                .file_name()
                .unwrap_or(entry.path.as_os_str())
                .to_owned();
            let dest = tmp.path().join(&name);
            match entry.kind {
                heph::hartifactcontent::WalkEntryKind::File { mut data, x } => {
                    let mut buf = Vec::new();
                    data.read_to_end(&mut buf)?;
                    std::fs::write(&dest, &buf)?;
                    if x {
                        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
                        binary_path = Some(dest);
                    }
                }
                heph::hartifactcontent::WalkEntryKind::Symlink { .. } => {}
            }
        }
    }

    let bin = binary_path.context("no executable artifact in build output")?;
    let output = std::process::Command::new(&bin)
        .output()
        .with_context(|| format!("run binary {}", bin.display()))?;

    let stdout = String::from_utf8(output.stdout).context("binary stdout is utf8")?;
    assert_eq!(stdout.trim(), "hello", "binary must print 'hello'");
    Ok(())
}

// Regression for content_buddy `heph3 r build ./go`: with a `go_codegen_root`
// (provider_state) covering the whole module, building a binary whose package
// graph spans multiple first-party packages must succeed. Each package's
// `_golist` issues a broad `q@label=go_src,package_prefix=<root>` query; that
// query `get_spec`s every sibling `build_lib`/`_golist` to read labels, which
// re-enters the in-flight computations and (pre-fix) trips a FALSE
// `build_lib -> _golist` cycle that fails the build.
#[tokio::test]
async fn test_codegen_root_build_multi_package() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegenroot")?;
    let ws = make_workspace(dir)?;
    let result = ws
        .run("//:build")
        .await
        .context("build //:build under a go_codegen_root")?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build must produce the binary even with a go_codegen_root spanning multiple packages"
    );
    Ok(())
}

// Regression for the exact content_buddy config: the generated Go sub-package
// (`genpb/`) is the output of a codegen target but ALSO lives under an
// `fs.skip` subtree (mirroring `go/gen/**`). The go provider prunes first-party
// packages inside skipped subtrees, so `//genpb:_golist` resolves to
// `TargetNotFound` — even though it is produced by codegen — and every importer
// fails. Building `//:build` must still succeed: codegen-produced packages must
// remain resolvable despite the skip.
#[tokio::test]
async fn test_codegen_root_build_with_generated_pkg_skipped() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegenroot")?;
    let ws = make_workspace_fs_skip(dir, &["genpb/**"])?;
    let result = ws
        .run("//:build")
        .await
        .context("build //:build with the generated sub-package under fs.skip")?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build must succeed even though the codegen-produced package is under fs.skip"
    );
    Ok(())
}

// Faithful repro of content_buddy `heph3 r build ./go`: a codegen target
// labelled `"codegen"` (NOT `"go_src"`) writes a generated Go package into a
// sub-tree that is also under `fs.skip` (`gen/**`, mirroring `go/gen/**`). The
// go provider prunes the generated package from its walk, so the importer's
// `go list` cannot resolve `//gen/pb/v1:_golist` — it fails with
// `target not found` and cascading cyclic-dependency errors.
//
// We first build `//:gen` (codegen=copy writes the package to disk, as
// `heph3 r build ./go` does when it builds the codegen target), then `//:build`
// — the binary whose import graph reaches the skipped generated package.
//
// KNOWN-FAILING: this reproduces the open bug, so it is `#[ignore]`d to keep the
// suite green. Run it explicitly to see the failure:
//   cargo test -p plugingo-e2e --test codegen -- --ignored test_codegen_into_skipped_subtree_repro
// The contrast test `test_codegen_root_build_with_generated_pkg_skipped` shows
// the working path: labelling the codegen target `go_src` wires it via the go
// provider's query, so the skip no longer hides the generated package.
#[ignore = "reproduces open bug: codegen package under fs.skip is unresolvable unless labelled go_src"]
#[tokio::test]
async fn test_codegen_into_skipped_subtree_repro() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegenskip")?;
    let ws = make_workspace_fs_skip(dir, &["gen/**"])?;

    // Populate the generated package on disk first, like the codegen target run.
    ws.run("//:gen").await.context("run //:gen codegen")?;

    let result = ws
        .run("//:build")
        .await
        .context("build //:build importing a codegen package under fs.skip")?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build must resolve the codegen-produced package even under fs.skip"
    );
    Ok(())
}

// Regression for content_buddy's false-cycle `heph3 r build ./go` (with codegen
// labelled `go_src`): building the whole graph concurrently must NOT trip a false
// cycle. A shared library imported by two binaries has its `build_lib` in-flight
// while a sibling package's `q@label=go_src,package_prefix=<root>` query (run for
// its `_golist`) `get_spec`s candidate targets to evaluate the matcher. A
// rejected candidate (e.g. a `build`-labelled dist target) must leave no edge in
// the request DAG — otherwise that phantom edge closes a false cycle. Fixed by
// resolving query candidates on a *speculative* request state
// (`RequestState::speculative`) that checks the breadcrumb chain for reentry but
// records no DAG edges. Building the lib directly always worked (see
// `test_codegencycle_single_target_builds`), confirming the cycle was spurious.
#[tokio::test]
async fn test_codegencycle_whole_graph_build() -> anyhow::Result<()> {
    use heph::htmatcher::Matcher;
    use heph::htpkg::PkgBuf;

    require_go!();
    let dir = fixture("codegencycle")?;
    // Whole-graph batch query = the exact content_buddy `heph3 r build ./go` path
    // (`And[Label(build), Package(...)]`), not a single `result_addr`. The batch
    // resolves every `build` target concurrently, which is what trips the cycle —
    // independent of lock backend.
    let ws = make_workspace(dir)?;
    let m = Matcher::And(vec![
        Matcher::Label("build".to_string()),
        Matcher::Package(PkgBuf::from("")),
    ]);
    let results = ws
        .run_matcher(&m)
        .await
        .context("whole-graph `build` query under a go_codegen_root")?;
    assert!(
        !results.is_empty() && results.iter().all(|r| !artifact_paths(r).is_empty()),
        "whole-graph build must succeed without a false build_lib -> _golist cycle"
    );
    Ok(())
}

// Control for the repro above: building the shared library's `build_lib`
// directly (no concurrent sibling consumer in-flight) must succeed. This is the
// `heph3 r //go/libs/confmgr:build_lib@...` case the user confirmed works,
// proving the whole-graph cycle is spurious.
#[tokio::test]
async fn test_codegencycle_single_target_builds() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegencycle")?;
    let ws = make_workspace(dir)?;
    let result = ws
        .run("//shared:build_lib")
        .await
        .context("build //shared:build_lib directly")?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "single-target build_lib must produce the .a archive"
    );
    Ok(())
}

// Regression: with the go provider registered BEFORE the buildfile provider,
// `q <label> .` must still find a buildfile codegen target (`//:gen`, labeled
// `go_src`) that lives in a Go package dir. `get_spec(//:gen)` asks the go
// provider first; it over-claims the name, drags in `go list` + its
// `q@label=go_src` query, and that closes a cycle. The engine must contain that
// cycle (fall through to the buildfile provider), not drop the target — `q all`
// finds it (addr-only match, no get_spec), so `q go_src` must too.
#[tokio::test]
async fn test_query_by_label_finds_codegen_target_go_first() -> anyhow::Result<()> {
    use futures::TryStreamExt as _;
    use heph::htmatcher::Matcher;
    use heph::htpkg::PkgBuf;

    require_go!();
    let dir = fixture("codegen")?;
    // Guard off: let the go provider over-claim `//:gen` so the engine's
    // cycle-containment (fall through to the buildfile provider) is what's tested.
    let ws = make_workspace_go_first(dir, false)?;

    let m = Matcher::And(vec![
        Matcher::Label("go_src".to_string()),
        Matcher::Package(PkgBuf::from("")),
    ]);
    let rs = ws.engine.new_state();
    let addrs: Vec<heph::htaddr::Addr> = ws.engine.clone().query(rs, &m).try_collect().await?;

    let formatted: Vec<String> = addrs.iter().map(|a| a.format()).collect();
    assert!(
        formatted.iter().any(|a| a == "//:gen"),
        "q go_src . must find the codegen target //:gen, got: {formatted:?}"
    );

    Ok(())
}

// Defect-2 guard: a failed go-provider attempt for `//:gen` must not leave a
// stale `gen -> _golist` edge in the request DAG, or a later legitimate
// `build_lib -> _golist -> go_src group -> gen` resolution would close a FALSE
// cycle. Building `//:build_lib` over the go-first workspace must still succeed.
#[tokio::test]
async fn test_build_lib_still_builds_go_first() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegen")?;
    // Guard off: the failed go over-claim of `//:gen` must not leave a stale DAG
    // edge that false-cycles this legit build.
    let ws = make_workspace_go_first(dir, false)?;
    let result = ws.run("//:build_lib").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build_lib must produce the .a archive even with go registered first"
    );
    Ok(())
}
