#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use common::Workspace;

// $TOOL_<GROUP> resolves to the sandbox bin/ symlink path and points at the
// tool's content.
#[tokio::test]
async fn test_tool_env_var_single() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "toolenv",
        r#"
target(name = "mybin", driver = "bash", run = "echo hi > $OUT", out = "mybin")
target(
    name = "consumer",
    driver = "bash",
    run = "echo $TOOL_MYBIN | grep -q '/bin/mybin$' && cat $TOOL_MYBIN > $OUT",
    out = "result.txt",
    tools = {"mybin": ["//toolenv:mybin"]},
)
"#,
    );

    let result = ws.run("//toolenv:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("hi"), "missing 'hi', got: {content:?}");
    Ok(())
}

// $TOOL (empty group) variant.
#[tokio::test]
async fn test_tool_env_var_empty_group() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "toolenvempty",
        r#"
target(name = "mybin", driver = "bash", run = "echo hi > $OUT", out = "mybin")
target(
    name = "consumer",
    driver = "bash",
    run = "echo $TOOL | grep -q '/bin/mybin$' && cat $TOOL > $OUT",
    out = "result.txt",
    tools = {"": ["//toolenvempty:mybin"]},
)
"#,
    );

    let result = ws.run("//toolenvempty:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("hi"), "missing 'hi', got: {content:?}");
    Ok(())
}

// $TOOL_<GROUP> with two tools in the same group: space-separated, both
// readable.
#[tokio::test]
async fn test_tool_env_var_multi_in_group() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "toolenvmulti",
        r#"
target(name = "a", driver = "bash", run = "echo aaa > $OUT", out = "a")
target(name = "b", driver = "bash", run = "echo bbb > $OUT", out = "b")
target(
    name = "consumer",
    driver = "bash",
    run = "for t in $TOOL_G; do cat $t; done > $OUT",
    out = "result.txt",
    tools = {"g": ["//toolenvmulti:a", "//toolenvmulti:b"]},
)
"#,
    );

    let result = ws.run("//toolenvmulti:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("aaa"), "missing 'aaa', got: {content:?}");
    assert!(content.contains("bbb"), "missing 'bbb', got: {content:?}");
    Ok(())
}

// Two distinct groups produce two distinct $TOOL_<G> vars.
#[tokio::test]
async fn test_tool_env_var_distinct_groups() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "toolenvgroups",
        r#"
target(name = "a", driver = "bash", run = "echo aaa > $OUT", out = "a")
target(name = "b", driver = "bash", run = "echo bbb > $OUT", out = "b")
target(
    name = "consumer",
    driver = "bash",
    run = "cat $TOOL_GA $TOOL_GB > $OUT",
    out = "result.txt",
    tools = {"ga": ["//toolenvgroups:a"], "gb": ["//toolenvgroups:b"]},
)
"#,
    );

    let result = ws.run("//toolenvgroups:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("aaa"), "missing 'aaa', got: {content:?}");
    assert!(content.contains("bbb"), "missing 'bbb', got: {content:?}");
    Ok(())
}

/// A tool that finds its own files relative to where it lives: `sdk/bin/hello`
/// reads `sdk/lib/data.txt` through `readlink -f "$0"`, the shape of an SDK
/// launcher. Its support files must land in the same tree as its outputs
/// (`exec_tools/`), so the layout between the two matches the producer's own
/// sandbox.
const SDK_TOOL_BUILD: &str = r#"
target(
    name = "tool",
    driver = "bash",
    run = [
        "mkdir -p sdk/bin sdk/lib",
        "echo hello > sdk/lib/data.txt",
        "printf '#!/bin/sh\\ncat \"$(dirname \"$(readlink -f \"$0\")\")/../lib/data.txt\"\\n' > sdk/bin/hello",
        "chmod +x sdk/bin/hello",
    ],
    out = {"hello": "sdk/bin/hello"},
    support_files = ["sdk/"],
)
"#;

#[tokio::test]
async fn tool_support_files_are_staged_next_to_the_tool() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("sdktool", SDK_TOOL_BUILD);
    ws.write_build_file(
        "sdkuse",
        r#"
target(
    name = "consumer",
    driver = "bash",
    tools = ["//sdktool:tool"],
    run = "hello > $OUT",
    out = "out.txt",
)
"#,
    );

    let result = ws.run("//sdkuse:consumer").await?;
    let content = common::artifact_string(&result);
    assert_eq!(content.trim(), "hello", "got: {content:?}");
    Ok(())
}

/// Shaped like `protoc`: a binary plus a data directory the consumer passes on
/// its command line. `greet` prints `$1/greeting.txt`.
const GREET_SDK_BUILD: &str = r#"
target(
    name = "sdk",
    driver = "bash",
    run = [
        "mkdir -p sdk/bin sdk/include",
        "echo hello > sdk/include/greeting.txt",
        "printf '#!/bin/sh\\ncat \"$1/greeting.txt\"\\n' > sdk/bin/greet",
        "chmod +x sdk/bin/greet",
    ],
    out = "sdk/bin/greet",
    support_files = "sdk/include/",
)
"#;

/// `$TOOL_ROOT` / `$TOOL_<G>_ROOT` name the tool's package directory as staged
/// for the consumer, so a path the tool's BUILD file declares (`sdk/include`)
/// is found without knowing where heph stages tools.
#[tokio::test]
async fn tool_root_names_the_tools_support_files() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("greetsdk", GREET_SDK_BUILD);
    ws.write_build_file(
        "greetuse",
        r#"
target(
    name = "unnamed",
    driver = "bash",
    tools = ["//greetsdk:sdk"],
    run = "greet $TOOL_ROOT/sdk/include > $OUT",
    out = "out.txt",
)
target(
    name = "named",
    driver = "bash",
    tools = {"my-sdk": ["//greetsdk:sdk"]},
    run = "[[ $TOOL_MY_SDK_ROOT == /* ]] && greet $TOOL_MY_SDK_ROOT/sdk/include > $OUT",
    out = "out.txt",
)
"#,
    );

    for addr in ["//greetuse:unnamed", "//greetuse:named"] {
        let result = ws.run(addr).await?;
        let content = common::artifact_string(&result);
        assert_eq!(content.trim(), "hello", "{addr}: got {content:?}");
    }
    Ok(())
}

/// One entry per tool package, sorted: two outputs of one tool share a root, a
/// second tool adds its own, and a workspace source file (no staged file under
/// its `@heph/fs` package) adds nothing.
#[tokio::test]
async fn tool_root_lists_each_tool_package_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "multi",
        r#"
target(
    name = "two",
    driver = "bash",
    run = "echo a > one; echo b > two",
    out = {"one": "one", "two": "two"},
)
"#,
    );
    ws.write_build_file("greetsdk", GREET_SDK_BUILD);
    ws.write_file("src/script.sh", "echo src\n");
    ws.write_build_file(
        "rootuse",
        r#"
target(
    name = "consumer",
    driver = "bash",
    tools = {"x": [
        "//multi:two|one",
        "//multi:two|two",
        "//greetsdk:sdk",
        "//@heph/fs:file@f=src/script.sh",
    ]},
    run = "for r in $TOOL_X_ROOT; do echo \"${r##*/}\"; done > $OUT",
    out = "out.txt",
)
"#,
    );

    let result = ws.run("//rootuse:consumer").await?;
    let content = common::artifact_string(&result);
    let roots: Vec<&str> = content.lines().collect();
    assert_eq!(roots, ["greetsdk", "multi"], "got {content:?}");
    Ok(())
}

/// A tool target for the collision tests: one file, nothing else.
const PLAIN_TOOL_BUILD: &str = r#"
target(name = "t", driver = "bash", run = "echo hi > $OUT", out = "t")
"#;

/// `tools = {"": …, "root": …}`: the unnamed group's root and group `root`'s
/// tool list are both `TOOL_ROOT`. Refused when the target is parsed, naming
/// both groups the way the BUILD file spells them.
#[tokio::test]
async fn tool_root_colliding_with_a_group_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("x", PLAIN_TOOL_BUILD);
    ws.write_build_file(
        "use",
        r#"
target(
    name = "c",
    driver = "bash",
    tools = {"": ["//x:t"], "root": ["//x:t"]},
    run = "echo $TOOL_ROOT > $OUT",
    out = "out.txt",
)
"#,
    );
    let Err(err) = ws.run("//use:c").await else {
        anyhow::bail!("built although TOOL_ROOT is claimed twice");
    };
    let err = format!("{err:#}");
    assert!(err.contains("`TOOL_ROOT`"), "{err}");
    assert!(err.contains("the unnamed group"), "{err}");
    assert!(err.contains("group \"root\""), "{err}");
    assert!(err.contains("//use:c"), "{err}");
    Ok(())
}

/// The target's own `env`, `runtime_env` or `runtime_pass_env` naming a root
/// heph sets is refused rather than silently replaced. (`pass_env` is refused
/// by its host snapshot, which needs the host to export the name:
/// `pluginexec`'s `tool_root_pass_env_snapshot_is_refused`, since `set_var` is
/// process-global and this binary is shared.)
#[tokio::test]
async fn tool_root_claimed_by_own_env_is_refused() -> anyhow::Result<()> {
    for (field, decl) in [
        ("env", r#"env = {"TOOL_ROOT": "/x"}"#),
        ("runtime_env", r#"runtime_env = {"TOOL_ROOT": "/x"}"#),
        ("runtime_pass_env", r#"runtime_pass_env = ["TOOL_ROOT"]"#),
    ] {
        let ws = Workspace::new();
        ws.write_build_file("x", PLAIN_TOOL_BUILD);
        ws.write_build_file(
            "use",
            &format!(
                r#"
target(
    name = "c",
    driver = "bash",
    tools = ["//x:t"],
    {decl},
    run = "echo $TOOL_ROOT > $OUT",
    out = "out.txt",
)
"#
            ),
        );
        let Err(err) = ws.run("//use:c").await else {
            anyhow::bail!("built although `{field}` claims TOOL_ROOT");
        };
        let err = format!("{err:#}");
        assert!(
            err.contains(&format!("`TOOL_ROOT` is set by the target's `{field}`")),
            "{field}: {err}"
        );
    }
    Ok(())
}

/// A group from a dependency's `transitive` tools collides like one of the
/// target's own: the unnamed group's root is `TOOL_ROOT`, which is also the
/// transitive group `root`'s tool list. Only `apply_transitive` sees both.
#[tokio::test]
async fn tool_root_colliding_with_a_transitive_group_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "x",
        r#"
target(name = "t", driver = "bash", run = "echo hi > $OUT", out = "t")
target(name = "u", driver = "bash", run = "echo hi > $OUT", out = "u")
"#,
    );
    ws.write_build_file(
        "use",
        r#"
target(
    name = "lib",
    driver = "bash",
    run = "echo lib > $OUT",
    out = "lib.txt",
    transitive = {"tools": {"root": ["//x:u"]}},
)
target(
    name = "c",
    driver = "bash",
    deps = ["//use:lib"],
    tools = ["//x:t"],
    run = "echo $TOOL_ROOT > $OUT",
    out = "out.txt",
)
"#,
    );
    let Err(err) = ws.run("//use:c").await else {
        anyhow::bail!("built although a transitive group claims TOOL_ROOT");
    };
    let err = format!("{err:#}");
    assert!(err.contains("`TOOL_ROOT`"), "{err}");
    assert!(err.contains("the unnamed group"), "{err}");
    assert!(err.contains("group \"root\""), "{err}");
    Ok(())
}

/// A dependency's `transitive` env literal reaches the consumer's env after
/// parse; it is refused once applied, saying where the name came from.
#[tokio::test]
async fn tool_root_claimed_by_transitive_env_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("x", PLAIN_TOOL_BUILD);
    ws.write_build_file(
        "use",
        r#"
target(
    name = "lib",
    driver = "bash",
    run = "echo lib > $OUT",
    out = "lib.txt",
    transitive = {"env": {"TOOL_ROOT": "/x"}},
)
target(
    name = "c",
    driver = "bash",
    deps = ["//use:lib"],
    tools = ["//x:t"],
    run = "echo $TOOL_ROOT > $OUT",
    out = "out.txt",
)
"#,
    );
    let Err(err) = ws.run("//use:c").await else {
        anyhow::bail!("built although a transitive env claims TOOL_ROOT");
    };
    let err = format!("{err:#}");
    assert!(err.contains("`TOOL_ROOT`"), "{err}");
    assert!(
        err.contains("`TOOL_ROOT` is set by a dependency's `transitive` env"),
        "{err}"
    );
    Ok(())
}

/// Prints each root of `$TOOL_SDKS_ROOT` relative to the staged tool tree, so
/// two consumers' values compare without their sandbox paths.
const PRINT_SDKS_ROOTS: &str =
    r#"for r in $TOOL_SDKS_ROOT; do echo "${r#*/exec_tools/}"; done > $OUT"#;

/// A `group` used as a tool contributes every member's package, and the value
/// does not depend on the order the group lists them in.
#[tokio::test]
async fn tool_root_of_a_group_tool_lists_every_member() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "a",
        r#"target(name = "t", driver = "bash", run = "echo a > $OUT", out = "a_tool")"#,
    );
    ws.write_build_file(
        "b",
        r#"target(name = "t", driver = "bash", run = "echo b > $OUT", out = "b_tool")"#,
    );
    ws.write_build_file(
        "t",
        &format!(
            r#"
target(name = "sdks", driver = "group", deps = ["//a:t", "//b:t"])
target(name = "sdks_rev", driver = "group", deps = ["//b:t", "//a:t"])
target(
    name = "c",
    driver = "bash",
    tools = {{"sdks": ["//t:sdks"]}},
    run = '{PRINT_SDKS_ROOTS}',
    out = "out.txt",
)
target(
    name = "c_rev",
    driver = "bash",
    tools = {{"sdks": ["//t:sdks_rev"]}},
    run = '{PRINT_SDKS_ROOTS}',
    out = "out.txt",
)
"#
        ),
    );

    let result = ws.run("//t:c").await?;
    let content = common::artifact_string(&result);
    assert_eq!(
        content.lines().collect::<Vec<_>>(),
        ["a", "b"],
        "got {content:?}"
    );
    let result = ws.run("//t:c_rev").await?;
    let reversed = common::artifact_string(&result);
    assert_eq!(reversed, content, "member order changed the value");
    Ok(())
}

/// A tool handed down through a dependency's `transitive` tools gets a root
/// in the consumer like a tool it declares itself.
#[tokio::test]
async fn tool_root_of_a_transitive_tool() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("greetsdk", GREET_SDK_BUILD);
    ws.write_build_file(
        "tr",
        r#"
target(
    name = "a",
    driver = "bash",
    run = "echo a > $OUT",
    out = "a.txt",
    transitive = {"tools": {"sdk": ["//greetsdk:sdk"]}},
)
target(
    name = "c",
    driver = "bash",
    deps = ["//tr:a"],
    run = "greet $TOOL_SDK_ROOT/sdk/include > $OUT",
    out = "out.txt",
)
"#,
    );
    let result = ws.run("//tr:c").await?;
    let content = common::artifact_string(&result);
    assert_eq!(content.trim(), "hello", "got {content:?}");
    Ok(())
}

/// A tool in the root package roots the whole staged tree, spelled without a
/// trailing slash.
#[tokio::test]
async fn tool_root_of_a_root_package_tool() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "",
        r#"
target(
    name = "tool",
    driver = "bash",
    run = [
        "mkdir -p data",
        "echo hello > data/greeting.txt",
        "echo tool > rtool",
    ],
    out = "rtool",
    support_files = "data/",
)
"#,
    );
    ws.write_build_file(
        "use",
        r#"
target(
    name = "c",
    driver = "bash",
    tools = ["//:tool"],
    run = 'case "$TOOL_ROOT" in */) exit 1;; esac; [[ "${TOOL_ROOT##*/}" == exec_tools ]] && cat $TOOL_ROOT/data/greeting.txt > $OUT',
    out = "out.txt",
)
"#,
    );
    let result = ws.run("//use:c").await?;
    let content = common::artifact_string(&result);
    assert_eq!(content.trim(), "hello", "got {content:?}");
    Ok(())
}

/// A host binary (`//@heph/bin:<name>`) is staged under `__heph/hostbin/`, not
/// under its package: it adds no root, alone or beside a tool that does. It is
/// still a staged tool, so alone it gets `$HEPH_TOOLS_DIR`.
#[tokio::test]
async fn tool_root_skips_a_host_binary() -> anyhow::Result<()> {
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                std::sync::Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(
            heph::pluginexec::Driver::new_bash().with_host_path(),
        ))
        .with_plugin(heph::pluginhostbin::PLUGIN_NAME, |_| {
            Ok(heph::engine::PluginParts::default()
                .with_provider(Box::new(heph::pluginhostbin::Provider))
                .with_driver(Box::new(heph::pluginhostbin::Driver)))
        })
        .build()?;
    ws.write_build_file("greetsdk", GREET_SDK_BUILD);
    ws.write_build_file(
        "use",
        r#"
target(
    name = "alone",
    driver = "bash",
    tools = {"h": ["//@heph/bin:sh"]},
    run = '[[ -z "${TOOL_H_ROOT+set}" && -n "$TOOL_H" && "$HEPH_TOOLS_DIR" == /* && -d "$HEPH_TOOLS_DIR" ]] && echo unset > $OUT',
    out = "out.txt",
)
target(
    name = "beside",
    driver = "bash",
    tools = {"x": ["//@heph/bin:sh", "//greetsdk:sdk"]},
    run = 'for r in $TOOL_X_ROOT; do echo "${r##*/}"; done > $OUT',
    out = "out.txt",
)
"#,
    );
    let result = ws.run("//use:alone").await?;
    let alone = htestkit::artifact_string(&result);
    assert_eq!(alone.trim(), "unset", "got {alone:?}");
    let result = ws.run("//use:beside").await?;
    let beside = htestkit::artifact_string(&result);
    assert_eq!(
        beside.lines().collect::<Vec<_>>(),
        ["greetsdk"],
        "got {beside:?}"
    );
    Ok(())
}

/// Shaped like an npm CLI wrapper: a launcher script that reads a file shipped
/// beside it, finding it through `$HEPH_TOOLS_DIR` and its own package path.
const GREET_LAUNCHER_BUILD: &str = r#"
target(
    name = "cli",
    driver = "bash",
    run = [
        "mkdir -p lib",
        "echo hello > lib/data.txt",
        "printf '#!/bin/sh\\ncat \"$HEPH_TOOLS_DIR/greetcli/lib/data.txt\"\\n' > greet",
        "chmod +x greet",
    ],
    out = "greet",
    support_files = "lib/",
)
"#;

/// `$HEPH_TOOLS_DIR` does not depend on how a consumer lists the tool: alone in
/// the unnamed group, beside another tool, under a named group, and reached
/// through another tool's `transitive` tools, the same launcher finds its
/// files. `$TOOL_ROOT` fails the second (a space-joined list) and the third
/// (unset).
#[tokio::test]
async fn tools_dir_finds_a_launchers_files_however_it_is_listed() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("greetcli", GREET_LAUNCHER_BUILD);
    ws.write_build_file("x", PLAIN_TOOL_BUILD);
    ws.write_build_file(
        "wrap",
        r#"
target(
    name = "wrap",
    driver = "bash",
    run = ["printf '#!/bin/sh\\nexec greet\\n' > wrap", "chmod +x wrap"],
    out = "wrap",
    transitive = {"tools": ["//greetcli:cli"]},
)
"#,
    );
    ws.write_build_file(
        "app",
        r#"
target(name = "only_tool", driver = "bash", run = "greet > $OUT",
       tools = ["//greetcli:cli"], out = "out.txt")
target(name = "two_tools", driver = "bash", run = "greet > $OUT",
       tools = ["//greetcli:cli", "//x:t"], out = "out.txt")
target(name = "named_group", driver = "bash", run = "greet > $OUT",
       tools = {"greeter": "//greetcli:cli"}, out = "out.txt")
target(name = "tool_of_a_tool", driver = "bash", run = "wrap > $OUT",
       tools = ["//wrap:wrap"], out = "out.txt")
"#,
    );

    for name in ["only_tool", "two_tools", "named_group", "tool_of_a_tool"] {
        let addr = format!("//app:{name}");
        let result = ws.run(&addr).await?;
        let content = common::artifact_string(&result);
        assert_eq!(content.trim(), "hello", "{addr}: got {content:?}");
    }
    Ok(())
}

/// heph owns `HEPH_TOOLS_DIR` for a target with tools: the target's own `env`
/// naming it is refused at parse rather than silently replaced. (The other
/// ways of setting it share `TOOL_<G>_ROOT`'s check, unit-tested in
/// `pluginexec`.)
#[tokio::test]
async fn tools_dir_claimed_by_own_env_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("x", PLAIN_TOOL_BUILD);
    ws.write_build_file(
        "use",
        r#"
target(
    name = "c",
    driver = "bash",
    tools = {"named": ["//x:t"]},
    env = {"HEPH_TOOLS_DIR": "/x"},
    run = "echo $HEPH_TOOLS_DIR > $OUT",
    out = "out.txt",
)
"#,
    );
    let Err(err) = ws.run("//use:c").await else {
        anyhow::bail!("built although `env` claims HEPH_TOOLS_DIR");
    };
    let err = format!("{err:#}");
    assert!(
        err.contains("`HEPH_TOOLS_DIR` is set by the target's `env`"),
        "{err}"
    );
    assert!(err.contains("//use:c"), "{err}");
    Ok(())
}

/// A target without tools is not heph's to police: its own `HEPH_TOOLS_DIR`
/// reaches its command unchanged.
#[tokio::test]
async fn tools_dir_is_the_targets_own_without_tools() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "use",
        r#"
target(
    name = "c",
    driver = "bash",
    env = {"HEPH_TOOLS_DIR": "/mine"},
    run = "echo $HEPH_TOOLS_DIR > $OUT",
    out = "out.txt",
)
"#,
    );
    let result = ws.run("//use:c").await?;
    let content = common::artifact_string(&result);
    assert_eq!(content.trim(), "/mine", "got {content:?}");
    Ok(())
}
