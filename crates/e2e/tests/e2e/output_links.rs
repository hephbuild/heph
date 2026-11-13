#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! Symlinks in a target's outputs, through the real exec driver and packer.
//!
//! Two rules (hephbuild/heph#563):
//! - an output glob never descends a symlinked directory; a target whose glob
//!   would fails, rather than packing what is behind the link (the host, an
//!   ancestor, a directory outside the output);
//! - a symlink packed into an artifact must be relative and resolve to a file or
//!   directory inside that same artifact.

use crate::common;

use common::Workspace;

/// `EResult` has no `Debug`, so `expect_err` will not compile.
fn expect_err<T>(r: anyhow::Result<T>, what: &str) -> anyhow::Error {
    match r {
        Ok(_) => panic!("{what}"),
        Err(e) => e,
    }
}

/// A `**` glob over a directory holding a link to `/` fails naming the link,
/// instead of walking the host.
#[tokio::test]
async fn an_output_glob_refuses_a_link_to_the_host_root() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "out/**/*.txt",
       run = ["mkdir -p out && echo hi > out/a.txt && ln -s / out/host"])"#,
    );
    let err = expect_err(
        ws.run("//app:a").await,
        "a glob through a link to / must fail",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("heph does not follow") && msg.contains("out/host"),
        "must name the link it refused: {msg}"
    );
    Ok(())
}

/// A link to a directory outside the output tree is not a way to pack files
/// from outside it.
#[tokio::test]
async fn an_output_glob_refuses_a_link_out_of_the_output() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "out/**/*.js",
       run = ["mkdir -p third_party/x out && echo x > third_party/x/lib.js",
              "ln -s ../third_party/x out/vendor"])"#,
    );
    let err = expect_err(
        ws.run("//app:a").await,
        "a glob through a link out must fail",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("heph does not follow") && msg.contains("out/vendor"),
        "must name the link it refused: {msg}"
    );
    Ok(())
}

/// A link to an ancestor fails at once, rather than recursing until the path
/// is too long.
#[tokio::test]
async fn an_output_glob_refuses_a_link_to_an_ancestor() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "out/**/*.txt",
       run = ["mkdir -p out/d && echo hi > out/d/a.txt && ln -s .. out/d/up"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "a glob through a loop must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("heph does not follow") && msg.contains("out/d/up"),
        "must name the link it refused: {msg}"
    );
    assert!(!msg.contains("too long"), "refused before recursing: {msg}");
    Ok(())
}

/// A link packed without what it points to fails the pack.
#[tokio::test]
async fn a_link_to_a_file_outside_the_artifact_fails_the_pack() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "l",
       run = ["echo hi > a.txt && ln -s a.txt l"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "a dangling link must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not in the artifact") && msg.contains("app/l"),
        "must name the link: {msg}"
    );
    Ok(())
}

/// A link that climbs out of the artifact fails the pack, even when the file
/// it names exists in the sandbox.
#[tokio::test]
async fn a_link_out_of_the_artifact_fails_the_pack() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "l",
       run = ["ln -s ../../../../../../../../etc/hosts l"])"#,
    );
    let err = expect_err(ws.run("//app:a").await, "an escaping link must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("escapes the artifact root"), "{msg}");
    Ok(())
}

/// Every file and link member of a result's artifacts, as `path` for a file
/// and `path -> target` for a link, in artifact order.
fn members(result: &heph::engine::EResult) -> Vec<String> {
    use heph::hartifactcontent::WalkEntryKind;
    let mut out = Vec::new();
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk artifact") {
            let entry = entry.expect("artifact entry");
            match entry.kind {
                WalkEntryKind::File { .. } => out.push(entry.path.display().to_string()),
                WalkEntryKind::Symlink { target } => {
                    out.push(format!("{} -> {}", entry.path.display(), target.display()))
                }
            }
        }
    }
    out
}

/// Links to a file or a directory the same artifact carries pack, including
/// a directory link matched (not descended) by a glob's last component.
#[tokio::test]
async fn links_inside_the_artifact_pack() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = ["l", "a.txt", "out/*", "out/real/*"],
       run = ["echo hi > a.txt && ln -s a.txt l",
              "mkdir -p out/real && echo b > out/real/b.txt && ln -s real out/inside"])"#,
    );
    let result = ws.run("//app:a").await?;
    assert_eq!(
        members(&result),
        [
            "app/l -> a.txt",
            "app/a.txt",
            "app/out/inside -> real",
            "app/out/real/b.txt",
        ],
        "the links are packed as links, beside what they point to"
    );
    Ok(())
}

/// A link's target must be in the link's own artifact: each output group is
/// its own tar, so a link to a file another group carries fails the pack.
#[tokio::test]
async fn a_link_to_a_file_in_another_output_group_fails_the_pack() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = {"link": "l", "file": "a.txt"},
       run = ["echo hi > a.txt && ln -s a.txt l"])"#,
    );
    let err = expect_err(
        ws.run("//app:a").await,
        "a link across output groups must fail",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not in the artifact") && msg.contains("app/l"),
        "must name the link: {msg}"
    );
    Ok(())
}

/// A declared file output is not read through a linked ancestor: with
/// `out/vendor` a link out of the output, `out/vendor/x.js` fails naming the
/// link rather than packing the file behind it.
#[tokio::test]
async fn a_declared_file_under_a_linked_directory_fails() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "out/vendor/x.js",
       run = ["mkdir -p third_party && echo x > third_party/x.js",
              "rm -rf out/vendor && ln -s ../third_party out/vendor"])"#,
    );
    let err = expect_err(
        ws.run("//app:a").await,
        "a file output through a linked ancestor must fail",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("heph does not follow") && msg.contains("app/out/vendor"),
        "must name the linked ancestor: {msg}"
    );
    Ok(())
}

/// The same for a declared directory output below a linked ancestor.
#[tokio::test]
async fn a_declared_directory_under_a_linked_directory_fails() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "out/vendor/lib/",
       run = ["mkdir -p third_party/lib && echo x > third_party/lib/x.js",
              "rm -rf out/vendor && ln -s ../third_party out/vendor"])"#,
    );
    let err = expect_err(
        ws.run("//app:a").await,
        "a directory output through a linked ancestor must fail",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("heph does not follow") && msg.contains("app/out/vendor"),
        "must name the linked ancestor: {msg}"
    );
    Ok(())
}
