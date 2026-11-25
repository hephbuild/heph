//! A test workspace leaves nothing behind in `$TMPDIR`.
//!
//! A run stages every tool read-only (`0555` dirs) under the workspace's home.
//! `tempfile::TempDir` cannot remove that, and its drop ignores the failure, so
//! each such test used to strand its whole workspace: hundreds of directories a
//! day, some holding a ~240 MB Go toolchain.

use crate::common::Workspace;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// The first directory under `root` that its owner cannot write to.
fn read_only_dir_under(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mode = std::fs::symlink_metadata(&dir).ok()?.permissions().mode();
        if mode & 0o200 == 0 {
            return Some(dir);
        }
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    None
}

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

    let root = ws.dir.path().to_path_buf();
    let sealed = read_only_dir_under(&root);
    assert!(
        sealed.is_some(),
        "the run staged nothing read-only under {}, so this test no longer covers the leak",
        root.display()
    );

    drop(ws);
    assert!(!root.exists(), "{} left behind", root.display());
}
