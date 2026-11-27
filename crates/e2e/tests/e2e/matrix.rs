#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `heph.core.matrix()` through the real engine: a matrix declares nothing of
//! its own (I1), so the targets its cells declare are the targets the same
//! calls written as a loop would declare, down to the def hash.

use futures::TryStreamExt as _;
use heph::engine::Discovery;
use std::collections::BTreeMap;
use std::sync::Arc;

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
        .build()
}

/// Every target of package `p`, by name, with its def hash.
async fn def_hashes(build: &str) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let ws = workspace()?;
    ws.write_build_file("p", build);
    let m = heph::htquery::parse("//p", &heph::htpkg::PkgBuf::from(""))?;
    let addrs: Vec<heph::htaddr::Addr> = Arc::clone(&ws.engine)
        .query(ws.engine.new_state(), &m, Discovery::Complete)
        .try_collect()
        .await?;
    let mut out = BTreeMap::new();
    for addr in addrs {
        let def = Arc::clone(&ws.engine)
            .get_def(ws.engine.new_state(), &addr)
            .await?;
        out.insert(addr.name.clone(), def.target_def.hash.clone());
    }
    Ok(out)
}

/// T5: targets declared by cells and by the same calls written as a loop on
/// that line have equal def hashes, and the package's targets are exactly what
/// the cells declared.
#[tokio::test]
async fn matrix_adds_nothing_to_def_hash() -> anyhow::Result<()> {
    let cells = def_hashes(
        r#"
def _cell(go, platform):
    n = "build." + go + "." + platform.replace("/", "-")
    target(name = n, driver = "bash", run = "echo " + go + " " + platform + " > $OUT", out = "o.txt")
    return ":" + n

B = heph.core.matrix(
    axes = {"go": ["1.22", "1.23"], "platform": ["linux/amd64", "darwin/arm64"]},
    cell = _cell,
    exclude = [{"go": "1.22", "platform": "darwin/arm64"}],
)
target(name = "all", driver = "bash", run = "cat $SRC > $OUT", out = "all.txt", deps = [c.value for c in B.cells])
"#,
    )
    .await?;
    let looped = def_hashes(
        r#"
def _cell(go, platform):
    n = "build." + go + "." + platform.replace("/", "-")
    target(name = n, driver = "bash", run = "echo " + go + " " + platform + " > $OUT", out = "o.txt")
    return ":" + n

B = [_cell(go, platform) for go, platform in [("1.22", "linux/amd64"), ("1.23", "linux/amd64"), ("1.23", "darwin/arm64")]]
target(name = "all", driver = "bash", run = "cat $SRC > $OUT", out = "all.txt", deps = B)
"#,
    )
    .await?;
    assert_eq!(
        cells.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "all",
            "build.1.22.linux-amd64",
            "build.1.23.darwin-arm64",
            "build.1.23.linux-amd64"
        ],
        "the package's targets are what the cells declared"
    );
    assert_eq!(cells, looped, "a matrix moves no def hash");
    Ok(())
}
