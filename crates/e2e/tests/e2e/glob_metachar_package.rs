//! Outputs of a target whose package path holds glob metacharacters — the
//! Next.js-style `app/[slug]`.
//!
//! An output path is the package joined with the user's `out`, and a glob
//! output is a pattern string. When the package went into that string
//! unescaped, `[slug]` was a character class matching one of `s l u g`, so the
//! pattern matched nothing in the real `app/[slug]` directory — and a plain
//! `out = "a.txt"` was classified as a glob on the strength of the package's
//! `[` alone. Only the user's part of an output is pattern text.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common;

use common::Workspace;
use std::path::PathBuf;

const PKG: &str = "app/[slug]";
const ADDR_PREFIX: &str = "//app/[slug]";

/// The artifact paths of `res`, sorted, as strings.
fn paths(res: &heph::engine::EResult) -> Vec<String> {
    let mut out: Vec<String> = common::artifact_paths(res)
        .into_iter()
        .map(|p: PathBuf| p.to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// `out = "a.txt"` is a file, not a pattern, whatever the package is called.
#[tokio::test]
async fn plain_file_output_is_packed() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "t",
    driver = "bash",
    out = "a.txt",
    run = "printf hello > a.txt",
)
"#,
    );

    let res = ws.run(&format!("{ADDR_PREFIX}:t")).await?;
    assert_eq!(paths(&res), vec!["app/[slug]/a.txt".to_string()]);
    assert_eq!(common::artifact_string(&res), "hello");
    Ok(())
}

/// `out = "*.txt"` matches the files the target wrote in its own package
/// directory: the package prefix is literal, only `*.txt` is a pattern.
#[tokio::test]
async fn glob_output_matches_inside_the_package() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "t",
    driver = "bash",
    out = "*.txt",
    run = "printf a > a.txt; printf b > b.txt; printf c > c.md",
)
"#,
    );

    let res = ws.run(&format!("{ADDR_PREFIX}:t")).await?;
    assert_eq!(
        paths(&res),
        vec![
            "app/[slug]/a.txt".to_string(),
            "app/[slug]/b.txt".to_string()
        ]
    );
    Ok(())
}

/// A directory output in such a package is packed too.
#[tokio::test]
async fn dir_output_is_packed() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "t",
    driver = "bash",
    out = "gen/",
    run = "mkdir -p gen; printf x > gen/x.txt",
)
"#,
    );

    let res = ws.run(&format!("{ADDR_PREFIX}:t")).await?;
    assert_eq!(paths(&res), vec!["app/[slug]/gen/x.txt".to_string()]);
    Ok(())
}

/// A `codegen = "copy"` glob output is written back into the real
/// `app/[slug]` directory.
#[tokio::test]
async fn codegen_copy_glob_output_writes_back() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "t",
    driver = "bash",
    out = "*.gen.txt",
    codegen = "copy",
    run = "printf generated > page.gen.txt",
)
"#,
    );

    ws.run(&format!("{ADDR_PREFIX}:t")).await?;
    let written = common::root(&ws).join("app/[slug]/page.gen.txt");
    assert_eq!(std::fs::read_to_string(&written)?, "generated");
    Ok(())
}

/// A target in `pkg` whose output lists, one per line and sorted, the files
/// its `src` dep group staged, as paths relative to the workspace root.
/// `set -f`: the paths hold `[`, and the shell must not glob them.
fn listing_build(src: &str) -> String {
    format!(
        r#"
target(
    name = "t",
    driver = "bash",
    deps = {{"src": [{src}]}},
    out = "out.txt",
    run = 'set -f; for f in $SRC_SRC; do echo "${{f#"$WORKSPACE_ROOT"/}}"; done | sort > $OUT',
)
"#
    )
}

/// BUILD `glob("*.txt")` reads its own package `app/[slug]`, not the sibling
/// `app/s` its unescaped `[slug]` would read as a class.
///
/// `glob()` returns its fs addr as a dep string
/// (`//@heph/fs:glob@p=app/\[slug\]/*.txt`), which `TargetAddr::parse` used to
/// split at the last `[` as a `[filter,…]` suffix and refuse as unclosed.
#[tokio::test]
async fn build_glob_reads_its_own_package() -> anyhow::Result<()> {
    for (pkg, near_miss) in [(PKG, "app/s/a.txt"), ("a[b", "ab/a.txt")] {
        let ws = Workspace::new();
        ws.write_file(&format!("{pkg}/a.txt"), "a");
        ws.write_file(near_miss, "near miss");
        ws.write_build_file(pkg, &listing_build(r#"glob("*.txt")"#));

        let res = ws.run(&format!("//{pkg}:t")).await?;
        assert_eq!(
            common::artifact_string(&res),
            format!("{pkg}/a.txt\n"),
            "package {pkg}"
        );
    }
    Ok(())
}

/// A dep on a target in a package holding `[` resolves, with and without a
/// filter list after its name.
#[tokio::test]
async fn a_dep_on_a_target_in_a_bracketed_package_resolves() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        PKG,
        r#"target(name = "d", driver = "bash", out = ["a.txt", "b.go"],
       run = "echo a > a.txt && echo b > b.go")"#,
    );
    ws.write_build_file(
        "use",
        &format!(
            r#"
target(name = "all", driver = "bash", deps = {{"d": "//{PKG}:d"}},
       out = "o", run = 'set -f; for f in $SRC_D; do echo "${{f#"$WORKSPACE_ROOT"/}}"; done | sort > o')
target(name = "txt", driver = "bash", deps = {{"d": "//{PKG}:d[**/*.txt]"}},
       out = "o", run = 'set -f; for f in $SRC_D; do echo "${{f#"$WORKSPACE_ROOT"/}}"; done | sort > o')
"#
        ),
    );
    let all = ws.run("//use:all").await?;
    assert_eq!(
        common::artifact_string(&all),
        format!("{PKG}/a.txt\n{PKG}/b.go\n")
    );
    let txt = ws.run("//use:txt").await?;
    assert_eq!(common::artifact_string(&txt), format!("{PKG}/a.txt\n"));
    Ok(())
}

/// The same for packages holding other glob or wax metacharacters: the
/// Next.js route group `app/(g)`, whose unescaped `(g)` is a wax group
/// matching the sibling `app/g`, and an unbalanced `]`.
#[tokio::test]
async fn build_glob_reads_packages_with_other_metachars() -> anyhow::Result<()> {
    for pkg in ["app/(g)", "a]b"] {
        let ws = Workspace::new();
        ws.write_file(&format!("{pkg}/a.txt"), "a");
        ws.write_file("app/g/a.txt", "near miss");
        ws.write_build_file(pkg, &listing_build(r#"glob("*.txt")"#));

        let res = ws.run(&format!("//{pkg}:t")).await?;
        assert_eq!(
            common::artifact_string(&res),
            format!("{pkg}/a.txt\n"),
            "package {pkg}"
        );
    }
    Ok(())
}

/// An exclude in package `a,b`: the escaped `\,` stays inside its pattern
/// when the glob addr's comma-joined excludes are split back.
#[tokio::test]
async fn build_glob_exclude_in_a_comma_package() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("a,b/x.ts", "x");
    ws.write_file("a,b/y.ts", "y");
    ws.write_build_file("a,b", &listing_build(r#"glob("*.ts", exclude = ["x.ts"])"#));

    let res = ws.run("//a,b:t").await?;
    assert_eq!(common::artifact_string(&res), "a,b/y.ts\n");
    Ok(())
}

/// `heph.fs.glob("*.txt")` in `app/[slug]` lists its own package's files,
/// package-relative; the near-miss `app/s` would come back root-relative.
#[tokio::test]
async fn heph_fs_glob_reads_its_own_package() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("app/[slug]/a.txt", "a");
    ws.write_file("app/s/b.txt", "near miss");
    ws.write_build_file(
        PKG,
        r#"
target(
    name = "t",
    driver = "bash",
    out = "out.txt",
    run = "set -f; printf '%s\\n' " + " ".join(heph.fs.glob("*.txt")) + " > $OUT",
)
"#,
    );

    let res = ws.run(&format!("{ADDR_PREFIX}:t")).await?;
    assert_eq!(common::artifact_string(&res), "a.txt\n");
    Ok(())
}
