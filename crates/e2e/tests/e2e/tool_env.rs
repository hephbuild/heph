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

/// One entry per tool package: two outputs of one tool share a root, a second
/// tool adds its own, and a workspace source file (no package of its own)
/// adds nothing.
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
    assert_eq!(roots, ["multi", "greetsdk"], "got {content:?}");
    Ok(())
}
