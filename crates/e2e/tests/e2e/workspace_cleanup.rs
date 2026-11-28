//! A test workspace leaves nothing behind in `$TMPDIR`.
//!
//! A run stages every tool read-only (`0555` dirs) under the workspace's home.
//! `tempfile::TempDir` cannot remove that, and its drop ignores the failure, so
//! each such test used to strand its whole workspace: hundreds of directories a
//! day, some holding a ~240 MB Go toolchain.

use crate::common::Workspace;
use std::os::unix::fs::PermissionsExt as _;

#[tokio::test]
async fn dropping_a_workspace_removes_the_tools_it_staged_read_only() {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"
target(
    name = "tool",
    driver = "bash",
    run = "printf '#!/bin/sh\necho ran\n' > $OUT && chmod +x $OUT",
    out = "tool.sh",
)
target(
    name = "use",
    driver = "bash",
    run = "$TOOL_T > $OUT",
    out = "out.txt",
    tools = {"t": ["//p:tool"]},
)
"#,
    );
    ws.run("//p:use").await.expect("run //p:use");

    // The run's own read-only staging lives in its sandbox, whose cleanup is
    // fire-and-forget: whether it is still on disk here is a race, and the
    // cleanup won it once in CI. So the test seals a tree the way staging does
    // (`0555` directories over a file) instead of relying on that one. The run
    // stays, so whatever it did leave behind must go too.
    let root = ws.dir.path().to_path_buf();
    let sealed = root.join("sealed");
    let inner = sealed.join("bin");
    std::fs::create_dir_all(&inner).expect("mkdir sealed/bin");
    std::fs::write(inner.join("tool"), "#!/bin/sh\n").expect("write tool");
    for dir in [&inner, &sealed] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("seal");
    }

    drop(ws);
    assert!(!root.exists(), "{} left behind", root.display());
}
