mod common;

use common::{artifact_paths, fixture, make_workspace, make_workspace_host, require_go};

#[tokio::test]
async fn test_simple_lib_build_lib() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("simple_lib")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//:build_lib").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "build_lib should produce at least one artifact"
    );
    Ok(())
}

#[tokio::test]
async fn test_with_dep_cmd_build() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_dep")?;
    let ws = make_workspace(dir)?;
    let result = ws.run("//cmd:build").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "cmd build should produce at least one artifact"
    );
    Ok(())
}

/// Same build, but with `gotool = "host"`: the provider uses the host `go`
/// (resolved from PATH / `go env GOROOT` in-sandbox) instead of a hermetic SDK.
#[tokio::test]
async fn test_with_dep_cmd_build_host_toolchain() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_dep")?;
    let ws = make_workspace_host(dir)?;
    let result = ws.run("//cmd:build").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "host-toolchain cmd build should produce at least one artifact"
    );
    Ok(())
}
