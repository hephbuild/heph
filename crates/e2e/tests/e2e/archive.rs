#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The builtin `archive` plugin end to end: `archive.create` packs two
//! targets' outputs, `archive.extract` unpacks the result, and the real engine
//! stages, hashes and caches both. See `docs/ARCHIVE.md`.

use heph::engine::PluginParts;
use heph::{pluginarchive, pluginbuildfile, pluginexec};
use std::io::Read as _;
use std::sync::Arc;

/// The builtin `archive` plugin, registered as `register_builtin_plugins`
/// does.
pub(crate) fn archive_plugin(builder: htestkit::WorkspaceBuilder) -> htestkit::WorkspaceBuilder {
    builder.with_plugin(pluginarchive::PLUGIN_NAME, |_| {
        Ok(PluginParts::default()
            .with_managed_driver(Box::new(pluginarchive::extract::Driver))
            .with_managed_driver(Box::new(pluginarchive::create::Driver)))
    })
}

/// A workspace with BUILD files, `bash` and the `archive` plugin.
fn workspace() -> anyhow::Result<htestkit::Workspace> {
    archive_plugin(
        htestkit::WorkspaceBuilder::new()?
            .with_provider(|init| {
                Box::new(pluginbuildfile::Provider::new(
                    init.root.to_path_buf(),
                    init.runtime.clone(),
                    Arc::clone(&init.functions),
                ))
            })
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path())),
    )
    .build()
}

/// Two producers under `p/out`, one of them an executable one directory
/// down, and a file outside it.
const PRODUCERS: &str = r##"
target(
    name = "a",
    driver = "bash",
    run = 'mkdir -p "$(dirname "$OUT")" && printf aye > "$OUT"',
    out = "out/a.txt",
)
target(
    name = "b",
    driver = "bash",
    run = 'mkdir -p "$(dirname "$OUT")" && printf "#!/bin/sh\necho bee\n" > "$OUT" && chmod +x "$OUT"',
    out = "out/sub/b.sh",
)
target(
    name = "stray",
    driver = "bash",
    run = 'printf stray > "$OUT"',
    out = "stray.txt",
)
"##;

/// Every file of a result, by path, with its contents and executable bit.
fn files(result: &heph::engine::EResult) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk") {
            let entry = entry.expect("entry");
            if let heph::hartifactcontent::WalkEntryKind::File { mut data, x, .. } = entry.kind {
                let mut s = String::new();
                data.read_to_string(&mut s).expect("utf8 file");
                out.push((entry.path.to_string_lossy().into_owned(), s, x));
            }
        }
    }
    out.sort();
    out
}

/// Every symlink of a result, by path, with its target.
fn links(result: &heph::engine::EResult) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk") {
            let entry = entry.expect("entry");
            if let heph::hartifactcontent::WalkEntryKind::Symlink { target } = entry.kind {
                out.push((
                    entry.path.to_string_lossy().into_owned(),
                    target.to_string_lossy().into_owned(),
                ));
            }
        }
    }
    out.sort();
    out
}

/// The output hashes of a result, one per artifact.
fn hashouts(result: &heph::engine::EResult) -> Vec<String> {
    result
        .artifacts_meta
        .iter()
        .map(|m| m.hashout.clone())
        .collect()
}

/// `create` → `extract` gives back the producers' files, laid out by
/// `strip_prefix`/`prefix`, with their executable bit, in every format.
#[tokio::test]
async fn create_then_extract_round_trips_in_every_format() -> anyhow::Result<()> {
    let ws = workspace()?;
    let mut build = PRODUCERS.to_string();
    for (name, format) in [
        ("tar", "tar"),
        ("tgz", "tar.gz"),
        ("tzst", "tar.zst"),
        ("zip", "zip"),
    ] {
        build.push_str(&format!(
            r#"
target(
    name = "pack_{name}",
    driver = "archive.create",
    deps = [":a", ":b"],
    strip_prefix = "p/out",
    prefix = "tool-1.0",
    out = "pack.{format}",
)
target(
    name = "unpacked_{name}",
    driver = "archive.extract",
    src = ":pack_{name}",
    format = "{format}",
    strip_components = 1,
)
"#
        ));
    }
    ws.write_build_file("p", &build);

    for name in ["tar", "tgz", "tzst", "zip"] {
        let result = ws.run(&format!("//p:unpacked_{name}")).await?;
        let dir = format!("p/unpacked_{name}");
        assert_eq!(
            files(&result),
            vec![
                (format!("{dir}/a.txt"), "aye".to_string(), false),
                (
                    format!("{dir}/sub/b.sh"),
                    "#!/bin/sh\necho bee\n".to_string(),
                    true
                ),
            ],
            "{name}"
        );
    }
    Ok(())
}

/// An `archive.extract` with no `format` reads it off the archive: here
/// from `src`'s file name, in every format.
#[tokio::test]
async fn extract_infers_an_omitted_format() -> anyhow::Result<()> {
    let ws = workspace()?;
    let mut build = PRODUCERS.to_string();
    for (name, format) in [
        ("tar", "tar"),
        ("tgz", "tar.gz"),
        ("tzst", "tar.zst"),
        ("zip", "zip"),
    ] {
        build.push_str(&format!(
            r#"
target(
    name = "pack_{name}",
    driver = "archive.create",
    deps = [":a"],
    strip_prefix = "p/out",
    out = "pack.{format}",
)
target(
    name = "unpacked_{name}",
    driver = "archive.extract",
    src = ":pack_{name}",
)
"#
        ));
    }
    ws.write_build_file("p", &build);

    for name in ["tar", "tgz", "tzst", "zip"] {
        let result = ws.run(&format!("//p:unpacked_{name}")).await?;
        assert_eq!(
            files(&result),
            vec![(format!("p/unpacked_{name}/a.txt"), "aye".to_string(), false)],
            "{name}"
        );
    }
    Ok(())
}

/// A dep's directory tree, with nested files, an empty file and a relative
/// symlink, comes back from `create` → `extract` as it went in.
#[tokio::test]
async fn a_tree_with_a_symlink_and_an_empty_file_round_trips() -> anyhow::Result<()> {
    let ws = workspace()?;
    let mut build = r#"
target(
    name = "tree",
    driver = "bash",
    run = 'mkdir -p tree/x/y && printf deep > tree/x/y/z.txt && printf top > tree/top.txt && : > tree/x/empty && ln -s y/z.txt tree/x/link',
    out = "tree/",
)
"#
    .to_string();
    for (name, format) in [("tgz", "tar.gz"), ("zip", "zip")] {
        build.push_str(&format!(
            r#"
target(
    name = "pack_{name}",
    driver = "archive.create",
    deps = [":tree"],
    strip_prefix = "p/tree",
    out = "pack.{format}",
)
target(
    name = "unpacked_{name}",
    driver = "archive.extract",
    src = ":pack_{name}",
)
"#
        ));
    }
    ws.write_build_file("p", &build);

    for name in ["tgz", "zip"] {
        let result = ws.run(&format!("//p:unpacked_{name}")).await?;
        let dir = format!("p/unpacked_{name}");
        assert_eq!(
            files(&result),
            vec![
                (format!("{dir}/top.txt"), "top".to_string(), false),
                (format!("{dir}/x/empty"), String::new(), false),
                (format!("{dir}/x/y/z.txt"), "deep".to_string(), false),
            ],
            "{name}"
        );
        assert_eq!(
            links(&result),
            vec![(format!("{dir}/x/link"), "y/z.txt".to_string())],
            "{name}"
        );
    }
    Ok(())
}

/// One file reached through two deps (directly and through a group) is
/// packed once, not refused as a clash nor written twice.
#[tokio::test]
async fn a_diamond_dep_is_packed_once() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"{PRODUCERS}
target(name = "g", driver = "group", deps = [":a"])
target(
    name = "pack",
    driver = "archive.create",
    deps = [":a", ":g"],
    strip_prefix = "p/out",
    out = "pack.tar",
)
target(
    name = "unpacked",
    driver = "archive.extract",
    src = ":pack",
)
"#
        ),
    );
    let result = ws.run("//p:unpacked").await?;
    assert_eq!(
        files(&result),
        vec![("p/unpacked/a.txt".to_string(), "aye".to_string(), false)]
    );
    Ok(())
}

/// `out` at the path a dep's file is staged at fails the target through the
/// engine, rather than truncating the dep's file and packing that.
#[tokio::test]
async fn create_refuses_an_out_a_dep_staged() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"{PRODUCERS}
target(
    name = "pack",
    driver = "archive.create",
    deps = [":stray"],
    format = "tar",
    out = "stray.txt",
)
"#
        ),
    );
    let Err(err) = ws.run("//p:pack").await else {
        anyhow::bail!("an `out` a dep staged must fail");
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("`out` \"stray.txt\" is the path a dep staged"),
        "{msg}"
    );
    Ok(())
}

/// The same deps packed in two fresh workspaces (fresh caches, producers run
/// again at different times) give byte-identical archives with the same
/// output hash: nothing about the run reaches the bytes.
#[tokio::test]
async fn create_is_reproducible_across_workspaces() -> anyhow::Result<()> {
    let build = format!(
        r#"{PRODUCERS}
target(
    name = "pack",
    driver = "archive.create",
    deps = [":a", ":b"],
    out = "pack.tar.gz",
)
"#
    );
    let mut runs = Vec::new();
    for _ in 0..2 {
        let ws = workspace()?;
        ws.write_build_file("p", &build);
        let result = ws.run("//p:pack").await?;
        runs.push((htestkit::artifact_bytes(&result), hashouts(&result)));
    }
    assert!(!runs[0].0.is_empty());
    assert_eq!(runs[0].0, runs[1].0, "archive bytes differ");
    assert_eq!(runs[0].1, runs[1].1, "output hashes differ");
    Ok(())
}

/// A packed file not under `strip_prefix` fails the target through the
/// engine, naming the file and the option.
#[tokio::test]
async fn create_refuses_a_file_outside_strip_prefix() -> anyhow::Result<()> {
    let ws = workspace()?;
    ws.write_build_file(
        "p",
        &format!(
            r#"{PRODUCERS}
target(
    name = "pack",
    driver = "archive.create",
    deps = [":a", ":stray"],
    strip_prefix = "p/out",
    out = "pack.tar",
)
"#
        ),
    );
    let Err(err) = ws.run("//p:pack").await else {
        anyhow::bail!("a file outside strip_prefix must fail");
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("1 of the staged files is not under `strip_prefix` \"p/out\":\n  p/stray.txt"),
        "{msg}"
    );
    assert!(
        msg.contains("did you mean `strip_prefix = \"p\"`?"),
        "{msg}"
    );
    Ok(())
}

/// One binary out of an unpacked archive is a tool: `archive.extract`'s
/// output is a directory, and a group with `include` picks the binary out
/// of it.
#[tokio::test]
async fn a_file_picked_out_of_an_extracted_archive_is_a_tool() -> anyhow::Result<()> {
    let ws = workspace()?;
    let mut build = PRODUCERS.to_string();
    build.push_str(
        r#"
target(
    name = "pack",
    driver = "archive.create",
    deps = [":a", ":b"],
    strip_prefix = "p/out",
    out = "pack.tar.gz",
)
target(name = "unpacked", driver = "archive.extract", src = ":pack")
target(name = "b_tool", driver = "group", deps = [":unpacked"], include = ["p/unpacked/sub/b.sh"])
target(
    name = "use",
    driver = "bash",
    run = ["b.sh > $OUT"],
    out = "out.txt",
    tools = [":b_tool"],
)
"#,
    );
    ws.write_build_file("p", &build);

    let result = ws.run("//p:use").await?;
    assert_eq!(
        files(&result),
        vec![("p/out.txt".to_string(), "bee\n".to_string(), false)]
    );
    Ok(())
}

/// An `archive.extract` used directly as a tool is held to one file when the
/// consumer runs: a one-file archive is a tool, a two-file one fails naming
/// the extract, and a `[filter]` on the reference that leaves one file is a
/// tool again — the count sees what staging places.
#[tokio::test]
async fn an_extracted_directory_is_a_tool_when_it_holds_one_file() -> anyhow::Result<()> {
    let ws = workspace()?;
    let mut build = PRODUCERS.to_string();
    build.push_str(
        r#"
target(name = "pack1", driver = "archive.create", deps = [":b"], strip_prefix = "p/out", out = "pack1.tar.gz")
target(name = "pack2", driver = "archive.create", deps = [":a", ":b"], strip_prefix = "p/out", out = "pack2.tar.gz")
target(name = "one", driver = "archive.extract", src = ":pack1")
target(name = "two", driver = "archive.extract", src = ":pack2")
[
    target(name = "use_" + n, driver = "bash", run = ["b.sh > $OUT"], out = "out.txt", tools = [t])
    for n, t in [("one", ":one"), ("two", ":two"), ("filtered", ":two[p/two/sub/b.sh]")]
]
"#,
    );
    ws.write_build_file("p", &build);

    for addr in ["//p:use_one", "//p:use_filtered"] {
        let result = ws.run(addr).await?;
        assert_eq!(
            files(&result),
            vec![("p/out.txt".to_string(), "bee\n".to_string(), false)],
            "{addr}"
        );
    }

    let err = ws
        .run("//p:use_two")
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("a two-file extract must not be a tool"))?;
    let msg = format!("{err:#}");
    assert!(
        msg.contains("tool target '//p:two' must produce exactly 1 file, found 2: p/two/a.txt, p/two/sub/b.sh"),
        "{msg}"
    );
    Ok(())
}
