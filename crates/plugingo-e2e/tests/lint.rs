#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::require_go;
use plugin_go::plugingo;

/// End-to-end lint against the *downloaded* heph-govet: the `http_fetch` target
/// fetches the released binary, the engine caches and stages it, and `_lint-analyze` execs
/// it as an `x/tools` unitchecker over a real package.
///
/// This is the shape that broke in production ("wait for heph-govet: Permission
/// denied"): nothing below the `_lint-analyze` target's own `run` was at fault — the bits
/// on the staged file are right — so the failure can only be reproduced by
/// actually exec'ing it, which is what this does.
///
/// The package imports only `unsafe`, so the release reads no archive. That keeps
/// the test about the download: the pinned release is a frozen tool, and what it
/// can read of the shell's Go (an older `go/types` panics on a newer export
/// format, as Go 1.27.2's was) or of the driver's cfg is no longer what ships.
/// The tests below cover that, with the tree's heph-govet.
#[tokio::test]
async fn test_lint_execs_the_downloaded_govet() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_host(common::fixture("lint_unsafe")?)?;

    // `_lint-analyze` is the analyze unit: it runs heph-govet and writes facts + report.
    let result = ws.run("//mem:_lint-analyze@v=host").await?;

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("lint.facts")),
        "analyze must produce serialized facts, got {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.ends_with("lint-report.json")),
        "analyze must produce a report, got {paths:?}"
    );
    Ok(())
}

/// Same for the format targets, which exec the same downloaded binary in its
/// `-format` mode: `format-check` reports, and must not rewrite anything.
#[tokio::test]
async fn test_format_check_execs_the_downloaded_govet() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_host(common::fixture("with_dep")?)?;

    // The fixture is gofmt-clean, so the check gate passes — the point is that it
    // gets far enough to actually run the tool. Bare addr: formatting is syntactic,
    // so `format`/`format-check` carry no variant.
    ws.run("//lib:format-check").await?;
    Ok(())
}

// The tests below lint with heph-govet built from this tree: they test the
// driver and the tool together, and the pinned release is a different tool. It
// was against the release that `_lint-analyze` stayed green while every package
// importing the standard library failed with the shipped heph-govet ("no package
// vetx file for \"fmt\""), from the x/tools bump until the fix in #530.

/// A leaf package importing only the standard library, which heph never lints and
/// so has no facts for: unitchecker still needs a vetx file for `fmt` to type-check.
#[tokio::test]
async fn test_lint_package_importing_stdlib() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_tree_govet(common::fixture("with_dep")?)?;

    let result = ws.run("//lib:_lint-analyze@v=host").await?;

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("lint.facts")),
        "analyze must produce facts, got {paths:?}"
    );
    Ok(())
}

/// A dependent package: its dep's facts feed the analysis. A package that exports
/// no facts writes a **zero-byte** `lint.facts` (x/tools `Set.Encode` returns nil),
/// and consuming that must be a no-op, not a decode failure.
#[tokio::test]
async fn test_lint_consumes_a_deps_empty_facts() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_tree_govet(common::fixture("with_dep")?)?;

    // cmd imports lib, so lib's facts are wired into cmd's analysis.
    let result = ws.run("//cmd:_lint-analyze@v=host").await?;

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("lint.facts")),
        "analyze must produce facts, got {paths:?}"
    );
    Ok(())
}

/// `unsafe` is a pseudo-package: no archive, so `go list` never reports it as a lib
/// and it gets no `lib_*` dep group. unitchecker nonetheless resolves every import
/// through the cfg's ImportMap, so a package importing `unsafe` fails with
/// `could not import unsafe (can't resolve import "")` unless the driver maps it.
#[tokio::test]
async fn test_lint_package_importing_unsafe() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_tree_govet(common::fixture("lint_unsafe")?)?;

    let result = ws.run("//mem:_lint-analyze@v=host").await?;

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("lint.facts")),
        "analyze must produce facts, got {paths:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `format` reads its sources through an fs glob (`format_sources_pattern`).
// These run the tree's heph-govet: no download, and the driver and the tool
// they test are the ones that ship together.
// ---------------------------------------------------------------------------

/// Not gofmt-clean: `format` rewrites it.
const UNFORMATTED: &str = "package lib\n\nfunc  Two( )  int {return 2}\n";
/// Not gofmt-clean either, so a `format` that staged it would rewrite it.
const GENERATED: &str = "package lib\n\nconst  Gen   =  1\n";

/// `with_dep`, plus an unformatted source in `lib` and, in that same package,
/// a `copy` generator writing `lib/gen.go` back into the tree.
fn format_workspace() -> anyhow::Result<htestkit::Workspace> {
    let ws = common::make_workspace_tree_govet(common::fixture("with_dep")?)?;
    ws.write_file("lib/two.go", UNFORMATTED);
    ws.write_build_file(
        "lib",
        &format!(
            r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf '{}' > $OUT",
    out = "gen.go",
    codegen = "copy",
)
"#,
            GENERATED.replace('\n', "\\\\n")
        ),
    );
    Ok(ws)
}

/// The paths a result's artifacts hold, as sorted strings.
fn paths_of(result: &heph::engine::EResult) -> Vec<String> {
    let mut v: Vec<String> = common::artifact_paths(result)
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    v.sort();
    v
}

fn read(ws: &htestkit::Workspace, rel: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;
    std::fs::read_to_string(ws.dir.path().join(rel)).with_context(|| format!("read {rel}"))
}

/// `format`'s `""` group is one fs glob over `<pkg>/*.go`, and the target
/// formats what that glob staged and writes it back in place.
#[tokio::test]
async fn go_format_stages_sources_through_fs_glob() -> anyhow::Result<()> {
    require_go!();
    let ws = format_workspace()?;

    let spec = ws.get_spec("//lib:format").await?;
    let Some(heph::htvalue::Value::Map(deps)) = spec.config.get("deps") else {
        panic!("deps not a map: {:?}", spec.config.get("deps"));
    };
    let glob = heph::pluginfs::glob_addr(&plugingo::format_sources_pattern("lib"), &[]).format();
    assert_eq!(
        format!("{:?}", deps.get("")),
        format!("Some(List([String({glob:?})]))"),
        "the sources are one fs glob, not one `file` per source"
    );

    let result = ws.run("//lib:format").await?;
    assert_eq!(paths_of(&result), vec!["lib/lib.go", "lib/two.go"]);
    let two = read(&ws, "lib/two.go")?;
    assert!(two.contains("func Two() int"), "rewritten in place: {two}");
    Ok(())
}

/// A `.go` file a `copy` generator wrote into the package is not a source: the
/// fs glob does not stage it, so `format` neither collects nor rewrites it.
#[tokio::test]
async fn go_format_skips_generated_go_files() -> anyhow::Result<()> {
    require_go!();
    let ws = format_workspace()?;
    ws.run("//lib:gen").await?;
    assert_eq!(read(&ws, "lib/gen.go")?, GENERATED);

    let result = ws.run("//lib:format").await?;
    assert_eq!(
        paths_of(&result),
        vec!["lib/lib.go", "lib/two.go"],
        "the generated file must not be staged or collected"
    );
    assert_eq!(
        read(&ws, "lib/gen.go")?,
        GENERATED,
        "generated file rewritten"
    );
    assert!(read(&ws, "lib/two.go")?.contains("func Two() int"));
    Ok(())
}

/// A package whose only `.go` files are generated has a `format` target (the
/// existence check excludes nothing), and it stages nothing and writes nothing.
#[tokio::test]
async fn go_format_of_only_generated_files_runs_nothing() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_tree_govet(common::fixture("with_dep")?)?;
    ws.write_build_file(
        "genonly",
        r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf 'package  genonly' > $OUT",
    out = "gen.go",
    codegen = "copy",
)
"#,
    );
    ws.run("//genonly:gen").await?;

    let result = ws.run("//genonly:format").await?;
    assert!(paths_of(&result).is_empty(), "{:?}", paths_of(&result));
    assert_eq!(read(&ws, "genonly/gen.go")?, "package  genonly");
    Ok(())
}

/// The `CacheOutcome`s `addr` reported on one fresh request.
async fn outcomes(
    ws: &htestkit::Workspace,
    addr: &str,
) -> anyhow::Result<Vec<hcore::events::CacheOutcomeKind>> {
    use heph::engine::event::BuildEventKind;
    let parsed = heph::htaddr::parse_addr(addr)?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = ws.engine.new_state_with_events(false, Some(tx));
    ws.engine
        .clone()
        .result_addr(
            rs,
            &parsed,
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await?;
    let mut v = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let BuildEventKind::CacheOutcome { addr: a, outcome } = &ev.kind
            && a == addr
        {
            v.push(*outcome);
        }
    }
    Ok(v)
}

/// The `in_place` output is the very glob the sources are staged through, so
/// the engine pairs them: the output is that glob, and after a run that
/// rewrote a file the fixpoint is stored, so the next run is a hit.
#[tokio::test]
async fn go_format_fixpoint_pairs_glob_input_with_glob_output() -> anyhow::Result<()> {
    require_go!();
    let ws = format_workspace()?;
    let addr = heph::htaddr::parse_addr("//lib:format")?;
    let def = ws
        .engine
        .clone()
        .get_def(ws.engine.new_state(), &addr)
        .await?;
    let glob = heph::pluginfs::glob_addr(&plugingo::format_sources_pattern("lib"), &[]);
    assert!(
        def.target_def.inputs.iter().any(|i| i.r#ref.r#ref == glob),
        "the glob is an input"
    );
    let outs: Vec<String> = def
        .target_def
        .outputs
        .iter()
        .flat_map(|o| o.paths.iter())
        .map(|p| format!("{:?}", p.content))
        .collect();
    assert_eq!(outs, vec![format!("Glob({:?})", "lib/*.go")]);

    assert_eq!(
        outcomes(&ws, "//lib:format").await?,
        vec![hcore::events::CacheOutcomeKind::Miss]
    );
    assert!(read(&ws, "lib/two.go")?.contains("func Two() int"));
    assert_eq!(
        outcomes(&ws, "//lib:format").await?,
        vec![hcore::events::CacheOutcomeKind::LocalHit],
        "the post-write-back tree must hit the stored fixpoint"
    );
    Ok(())
}

/// `<pkg>`'s glob metacharacters are escaped: the fs glob `format` stages its
/// sources through, built for a directory named with `[ ] { } < >`, survives
/// the addr round trip and the fs walker and stages that directory's `.go`
/// files only. Through the fs target rather than `//<pkg>:format`, because the
/// Go provider serves no target for such a directory (no Go import path can
/// hold those characters). `\` has no wax 0.7 spelling; see
/// `format_sources_pattern_escapes_the_package`.
#[tokio::test]
async fn go_format_escapes_glob_metachars_in_pkg() -> anyhow::Result<()> {
    let ws = common::make_workspace_tree_govet(common::fixture("with_dep")?)?;
    let pkg = "odd/we[i]rd{x}<y>";
    ws.write_file(&format!("{pkg}/a.go"), "package odd\n");
    ws.write_file(&format!("{pkg}/notes.txt"), "not go\n");
    // Siblings the unescaped `[i]` class or `{x}` alternative would reach.
    ws.write_file("odd/weirdx<y>/a.go", "package odd\n");
    ws.write_file("odd/we[i]rdx<y>/a.go", "package odd\n");

    let glob = heph::pluginfs::glob_addr(&plugingo::format_sources_pattern(pkg), &[]);
    let roundtrip = heph::htaddr::parse_addr(&glob.format())?;
    assert_eq!(roundtrip, glob, "the addr must survive format/parse");
    let result = ws.run_addr(roundtrip).await?;
    assert_eq!(paths_of(&result), vec![format!("{pkg}/a.go")]);
    Ok(())
}

/// The cache key does not move: `format` over glob-staged sources computes the
/// same `hashin` as the per-file `file()` shape it replaced, so a package whose
/// source set is unchanged hits the entry an older heph wrote.
#[tokio::test]
async fn go_format_unchanged_sources_hit_old_cache_entry() -> anyhow::Result<()> {
    use heph::htvalue::Value;
    require_go!();
    // The tool and config addrs, read from the new spec. A BUILD file is
    // evaluated once per engine, so the old-shape target goes in a second one.
    let probe = common::make_workspace_tree_govet(common::fixture("with_dep")?)?;
    let spec = probe.get_spec("//lib:format").await?;
    let ws = common::make_workspace_tree_govet(common::fixture("with_dep")?)?;
    ws.write_file("lib/two.go", UNFORMATTED);

    let Some(Value::Map(deps)) = spec.config.get("deps") else {
        panic!("deps not a map: {:?}", spec.config.get("deps"));
    };
    let group = |name: &str| -> String {
        let Some(Value::List(v)) = deps.get(name) else {
            panic!("group {name}: {:?}", deps.get(name));
        };
        v.iter()
            .map(|x| match x {
                Value::String(s) => format!("{s:?}"),
                other => panic!("not a string: {other:?}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let old_sources = ["lib/lib.go", "lib/two.go"]
        .iter()
        .map(|f| format!("{:?}", heph::pluginfs::file_addr(f).format()))
        .collect::<Vec<_>>()
        .join(", ");
    ws.write_build_file(
        "lib",
        &format!(
            r#"
target(
    name = "old_format",
    driver = "go.format",
    deps = {{"": [{old_sources}], "govet_tool": [{}], "config": [{}]}},
)
"#,
            group("govet_tool"),
            group("config")
        ),
    );

    let hashin = |a: &'static str| {
        let engine = ws.engine.clone();
        async move {
            let addr = heph::htaddr::parse_addr(a)?;
            anyhow::Ok(engine.clone().meta(engine.new_state(), &addr).await?.hashin)
        }
    };
    assert_eq!(
        hashin("//lib:format").await?,
        hashin("//lib:old_format").await?
    );
    Ok(())
}
