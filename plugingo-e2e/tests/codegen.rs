mod common;

use anyhow::Context as _;
use common::{
    artifact_paths, artifact_string, fixture, make_workspace, make_workspace_go_first, require_go,
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
