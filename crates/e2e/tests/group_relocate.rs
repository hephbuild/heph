//! End-to-end coverage for `group` in filter/relocate mode — re-exporting
//! other targets' outputs at different paths without copying their bytes.
//!
//! The properties worth freezing here are the ones no unit test can reach:
//! that a relocated file actually lands at the new path *inside a consuming
//! target's sandbox*, that changing a transform invalidates that consumer's
//! cache key, and that the group itself still writes nothing to the cache.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::Workspace;
use heph::htaddr::parse_addr;

/// Output paths are workspace-relative (package-prefixed), so transforms are
/// written against `<pkg>/…`. That is what lets one group aggregate targets
/// from several packages without an ambiguous "which package do I strip".
const PKG: &str = "reloc";

async fn hashin(ws: &Workspace, addr: &str) -> anyhow::Result<String> {
    let rs = ws.engine.new_state();
    let meta = ws
        .engine
        .clone()
        .meta(rs, &parse_addr(addr)?)
        .await?;
    Ok(meta.hashin)
}

/// The headline behaviour: a file produced at `build/out/server` is visible to
/// a consumer at `lib/server`.
#[tokio::test]
async fn relocated_output_reaches_the_consumer_sandbox_at_the_new_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "mkdir -p build/out && echo server_bytes > build/out/server",
    out = "build/out/server",
)
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:bin"],
    strip_prefix = "reloc/build/out",
    prefix = "lib",
)
target(
    name = "consumer",
    driver = "bash",
    # `$SRC_DIST` is the materialized path inside the sandbox, so this asserts
    # both that the bytes arrived and that they arrived at the relocated path.
    run = ["{ cat \"$SRC_DIST\"; echo \"at=$SRC_DIST\"; } > $OUT"],
    out = "result.txt",
    deps = {"dist": ["//reloc:dist"]},
)
"#,
    );

    let result = ws.run("//reloc:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("server_bytes"),
        "relocated file's bytes did not arrive: {content:?}"
    );
    assert!(
        content.contains("/ws/lib/server\n") || content.ends_with("/ws/lib/server"),
        "expected the file to materialize at ws/lib/server, got: {content:?}"
    );
    Ok(())
}

#[tokio::test]
async fn group_artifact_paths_are_the_rewritten_ones() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "mkdir -p build/out/sub && echo a > build/out/server && echo b > build/out/sub/x.so",
    out = ["build/out/server", "build/out/sub/x.so"],
)
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:bin"],
    strip_prefix = "reloc/build/out",
    prefix = "lib",
)
"#,
    );

    let result = ws.run("//reloc:dist").await?;
    let mut paths: Vec<String> = common::artifact_paths(&result)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["lib/server", "lib/sub/x.so"]);
    Ok(())
}

#[tokio::test]
async fn include_globs_drop_the_rest() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "echo a > lib.so && echo b > notes.txt",
    out = ["lib.so", "notes.txt"],
)
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:bin"],
    include = ["**/*.so"],
)
"#,
    );

    let result = ws.run("//reloc:dist").await?;
    let paths: Vec<String> = common::artifact_paths(&result)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(paths, vec!["reloc/lib.so"]);
    Ok(())
}

#[tokio::test]
async fn rename_places_a_file_verbatim() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(name = "bin", driver = "bash", run = "echo a > server", out = "server")
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:bin"],
    rename = {"reloc/server": "bin/myserver"},
)
"#,
    );

    let result = ws.run("//reloc:dist").await?;
    let paths: Vec<String> = common::artifact_paths(&result)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(paths, vec!["bin/myserver"]);
    Ok(())
}

/// The cache-correctness property the design rests on. A transparent group is
/// expanded away before `hashin` is computed, so if a relocating group stayed
/// transparent its transform would never reach a consumer's cache key and the
/// consumer would reuse an entry built against the old layout.
#[tokio::test]
async fn changing_the_transform_invalidates_the_consumers_cache_key() -> anyhow::Result<()> {
    let build = |prefix: &str| {
        format!(
            r#"
target(name = "bin", driver = "bash", run = "echo a > server", out = "server")
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:bin"],
    prefix = "{prefix}",
)
target(
    name = "consumer",
    driver = "bash",
    run = "find . -type f > $OUT",
    out = "result.txt",
    deps = {{"dist": ["//reloc:dist"]}},
)
"#
        )
    };

    let ws = Workspace::new();
    ws.write_build_file(PKG, &build("lib"));
    let with_lib = hashin(&ws, "//reloc:consumer").await?;

    let ws2 = Workspace::new();
    ws2.write_build_file(PKG, &build("bin"));
    let with_bin = hashin(&ws2, "//reloc:consumer").await?;

    assert_ne!(
        with_lib, with_bin,
        "a consumer's hashin must change when its dep group's `prefix` changes, \
         or it would reuse an entry built against the old paths"
    );
    Ok(())
}

/// The flip side: the same transform must hash identically, or every build
/// would miss cache.
#[tokio::test]
async fn an_unchanged_transform_keeps_the_cache_key_stable() -> anyhow::Result<()> {
    let src = r#"
target(name = "bin", driver = "bash", run = "echo a > server", out = "server")
target(name = "dist", driver = "group", deps = ["//reloc:bin"], prefix = "lib")
target(
    name = "consumer",
    driver = "bash",
    run = "find . -type f > $OUT",
    out = "result.txt",
    deps = {"dist": ["//reloc:dist"]},
)
"#;
    let ws = Workspace::new();
    ws.write_build_file(PKG, src);
    let a = hashin(&ws, "//reloc:consumer").await?;

    let ws2 = Workspace::new();
    ws2.write_build_file(PKG, src);
    let b = hashin(&ws2, "//reloc:consumer").await?;

    assert_eq!(a, b, "an unchanged transform must hash stably");
    Ok(())
}

/// The whole point of the feature: relocating costs no stored bytes. A
/// relocating group is a real (non-transparent) target, but it must still be
/// uncacheable so no revision or blob is ever written for it.
#[tokio::test]
async fn a_relocating_group_is_concrete_but_never_cached() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(name = "bin", driver = "bash", run = "echo a > server", out = "server")
target(name = "dist", driver = "group", deps = ["//reloc:bin"], prefix = "lib")
"#,
    );

    let rs = ws.engine.new_state();
    let def = ws
        .engine
        .clone()
        .get_def(rs, &parse_addr("//reloc:dist")?)
        .await?;

    assert!(
        !def.target_def.transparent,
        "a relocating group must be concrete, or its transform escapes consumers' cache keys"
    );
    assert!(
        !def.target_def.cache.enabled,
        "a relocating group owns no bytes and must never write a cache entry"
    );
    Ok(())
}

/// No-regression guard: a group with no transform keeps its original
/// zero-cost, inlined behaviour.
#[tokio::test]
async fn a_plain_group_is_still_transparent() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(name = "bin", driver = "bash", run = "echo a > server", out = "server")
target(name = "dist", driver = "group", deps = ["//reloc:bin"])
"#,
    );

    let rs = ws.engine.new_state();
    let def = ws
        .engine
        .clone()
        .get_def(rs, &parse_addr("//reloc:dist")?)
        .await?;
    assert!(def.target_def.transparent);
    assert!(!def.target_def.cache.enabled);
    Ok(())
}

/// A typo'd `strip_prefix` must fail the build and say what was available,
/// rather than silently passing every path through untouched.
#[tokio::test]
async fn a_transform_matching_nothing_fails_with_the_available_paths() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(name = "bin", driver = "bash", run = "echo a > server", out = "server")
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:bin"],
    strip_prefix = "nowhere/at/all",
)
"#,
    );

    let err = match ws.run("//reloc:dist").await {
        Ok(_) => panic!("expected a typo'd strip_prefix to fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("nowhere/at/all"), "{err}");
    assert!(
        err.contains("reloc/server"),
        "error should list the real paths: {err}"
    );
    Ok(())
}

/// Two files landing on one destination must fail rather than one silently
/// clobbering the other.
#[tokio::test]
async fn colliding_destinations_fail_the_build() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "mkdir -p a && echo one > a/x && echo two > x",
    out = ["a/x", "x"],
)
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:bin"],
    # Both sources are sent to the same destination.
    rename = {"reloc/a/x": "out/x", "reloc/x": "out/x"},
)
"#,
    );

    let err = match ws.run("//reloc:dist").await {
        Ok(_) => panic!("expected colliding destinations to fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("collision"), "{err}");
    Ok(())
}

/// Relocation composes with the rest of the graph: a group over two producers
/// in different packages, where the prefix only applies to one of them.
#[tokio::test]
async fn a_group_can_relocate_across_packages() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "mkdir -p build && echo app_bytes > build/server",
    out = "build/server",
)
"#,
    );
    ws.write_build_file(
        "web",
        r#"
target(name = "assets", driver = "bash", run = "echo css_bytes > site.css", out = "site.css")
target(
    name = "dist",
    driver = "group",
    deps = ["//app:bin", "//web:assets"],
    strip_prefix = "app/build",
    prefix = "release",
)
target(
    name = "consumer",
    driver = "bash",
    # `$LIST_SRC_DIST` lists every materialized path for the group.
    run = ["cat \"$LIST_SRC_DIST\" > $OUT"],
    out = "result.txt",
    deps = {"dist": ["//web:dist"]},
)
"#,
    );

    let result = ws.run("//web:consumer").await?;
    let listed = common::artifact_string(&result);
    assert!(
        listed.contains("/ws/release/server"),
        "app/build/server should relocate to release/server, got: {listed:?}"
    );
    assert!(
        listed.contains("/ws/release/web/site.css"),
        "web/site.css is not under the stripped prefix and should only gain \
         the prefix, got: {listed:?}"
    );
    Ok(())
}

/// An author writing a group must not have to know where a dependency emitted
/// its outputs. Heph emits `<pkg>/<declared out path>`, so the full path here
/// is `app/build/out/server` — a spelling that leaks the dep's package *and*
/// its internal build layout, and that breaks if the dep ever moves package.
/// The string form of `rename` names no source at all.
#[tokio::test]
async fn string_rename_needs_no_knowledge_of_the_emitted_path() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "mkdir -p build/out && echo v > build/out/server",
    out = "build/out/server",
)
"#,
    );
    ws.write_build_file(
        "dist",
        r#"
target(
    name = "dist",
    driver = "group",
    deps = ["//app:bin"],
    rename = "bin/myserver",
)
"#,
    );

    let result = ws.run("//dist:dist").await?;
    let paths: Vec<String> = common::artifact_paths(&result)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(paths, vec!["bin/myserver"]);
    Ok(())
}

/// `include` narrows a multi-output dep down to the one file the string form
/// needs — the documented fix when it reports too many candidates.
#[tokio::test]
async fn include_narrows_a_string_rename_to_one_output() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "echo a > server.so && echo b > notes.txt",
    out = ["server.so", "notes.txt"],
)
target(
    name = "dist",
    driver = "group",
    deps = ["//app:bin"],
    include = ["**/*.so"],
    rename = "lib/libserver.so",
)
"#,
    );

    let result = ws.run("//app:dist").await?;
    let paths: Vec<String> = common::artifact_paths(&result)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(paths, vec!["lib/libserver.so"]);
    Ok(())
}

/// Same constraint for `strip_prefix`: it is written the way the *producing*
/// BUILD file declares its output (`build/out`), not the way heph emits it
/// (`app/build/out`). Both spellings must land on the same layout.
#[tokio::test]
async fn strip_prefix_is_written_as_the_producer_declares_it() -> anyhow::Result<()> {
    let build = |prefix: &str| {
        format!(
            r#"
target(
    name = "dist",
    driver = "group",
    deps = ["//app:bin"],
    strip_prefix = "{prefix}",
    prefix = "release",
)
"#
        )
    };
    let producer = r#"
target(
    name = "bin",
    driver = "bash",
    run = "mkdir -p build/out && echo v > build/out/server",
    out = "build/out/server",
)
"#;

    let paths_for = async |prefix: &str| -> anyhow::Result<Vec<String>> {
        let ws = Workspace::new();
        ws.write_build_file("app", producer);
        ws.write_build_file("dist", &build(prefix));
        let result = ws.run("//dist:dist").await?;
        Ok(common::artifact_paths(&result)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect())
    };

    // Written the way the producer declares it — no `app/` required.
    assert_eq!(paths_for("build/out").await?, vec!["release/server"]);
    // And the fully-qualified spelling agrees.
    assert_eq!(paths_for("app/build/out").await?, vec!["release/server"]);
    Ok(())
}

/// One destination and two candidate outputs must fail loudly, naming both and
/// pointing at the fix, rather than silently picking one.
#[tokio::test]
async fn a_string_rename_with_two_outputs_fails_naming_the_candidates() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "mkdir -p a b && echo one > a/server && echo two > b/server",
    out = ["a/server", "b/server"],
)
target(
    name = "dist",
    driver = "group",
    deps = ["//app:bin"],
    rename = "bin/s",
)
"#,
    );

    let err = match ws.run("//app:dist").await {
        Ok(_) => panic!("expected an over-broad string rename to fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("app/a/server"), "{err}");
    assert!(err.contains("app/b/server"), "{err}");
    assert!(err.contains("include"), "should suggest narrowing: {err}");
    Ok(())
}

/// A member's `transitive` sandbox must still reach the consumer once the
/// group stops being transparent.
///
/// This works because `collect_transitive_deps` branches on the *driver name*
/// (`group`), not on `transparent` — so both modes take the pre-computed
/// `applied_transitive` path. That is easy to break by "tidying" the branch
/// into a `transparent` check, which is exactly why it is pinned here.
/// Transitive deps land at their own paths; the transform re-exports outputs,
/// and is not meant to move them.
#[tokio::test]
async fn transitive_deps_still_propagate_through_a_relocating_group() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(name = "tool", driver = "bash", run = "echo tool_value > $OUT", out = "tool.txt")
target(
    name = "lib",
    driver = "bash",
    run = "echo lib_value > $OUT",
    out = "lib.txt",
    transitive = {"deps": {"tool": ["//reloc:tool"]}},
)
target(
    name = "dist",
    driver = "group",
    deps = ["//reloc:lib"],
    prefix = "release",
)
target(
    name = "consumer",
    driver = "bash",
    run = ["printf '%s %s' \"$(cat \"$SRC_DIST\")\" \"$(cat \"$SRC_TOOL\")\" > $OUT"],
    out = "result.txt",
    deps = {"dist": ["//reloc:dist"]},
)
"#,
    );

    let result = ws.run("//reloc:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("lib_value"),
        "relocated output missing: {content:?}"
    );
    assert!(
        content.contains("tool_value"),
        "transitive dep did not survive the group becoming concrete: {content:?}"
    );
    Ok(())
}

/// Inline dep filters (`//foo:bar[…]`) accept globs, not just exact paths.
#[tokio::test]
async fn inline_dep_filters_accept_globs() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "echo a > keep.so && echo b > drop.txt",
    out = ["keep.so", "drop.txt"],
)
target(
    name = "consumer",
    driver = "bash",
    # cwd is the sandbox's package dir, so the dep's files are alongside.
    run = "test ! -e drop.txt && cat keep.so > $OUT",
    out = "result.txt",
    deps = {"bin": ["//reloc:bin[**/*.so]"]},
)
"#,
    );

    let result = ws.run("//reloc:consumer").await?;
    assert!(
        common::artifact_string(&result).contains("a"),
        "glob filter should have exposed keep.so and hidden drop.txt"
    );
    Ok(())
}

/// The compatibility guarantee for inline filters: an exact path keeps working
/// exactly as it did before they became glob-aware.
#[tokio::test]
async fn inline_dep_filters_still_accept_exact_paths() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "bin",
    driver = "bash",
    run = "echo a > keep.txt && echo b > drop.txt",
    out = ["keep.txt", "drop.txt"],
)
target(
    name = "consumer",
    driver = "bash",
    run = "test ! -e drop.txt && cat keep.txt > $OUT",
    out = "result.txt",
    deps = {"bin": ["//reloc:bin[reloc/keep.txt]"]},
)
"#,
    );

    let result = ws.run("//reloc:consumer").await?;
    assert!(common::artifact_string(&result).contains("a"));
    Ok(())
}
