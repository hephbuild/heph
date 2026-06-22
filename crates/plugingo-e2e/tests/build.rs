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
    let result = ws.run("//:build_lib").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "embedding build_lib should compile and produce an archive"
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
    let lib = "//@heph/go/thirdparty/github.com/klauspost/cpuid/v2@v2.2.5:build_lib";
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
    let result = ws.run("//cmd:build").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "host-toolchain cmd build should produce at least one artifact"
    );
    Ok(())
}
