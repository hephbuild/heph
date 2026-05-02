mod common;

use common::Workspace;
use rheph::engine::ResultOptions;
use rheph::htmatcher::Matcher;
use rheph::htpkg::PkgBuf;
use rheph::pluginstatictarget::Target;

fn bash(addr: &str, run: &str, out: &str) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: Some(out.to_string()),
        labels: vec![],
    }
}

#[tokio::test]
async fn test_static_target_executes() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![bash(
        "//hello:greet",
        "printf 'hello' > $OUT",
        "out.txt",
    )])?;

    let result = ws.run("//hello:greet").await?;
    assert!(artifact_string(&result).contains("hello"));
    Ok(())
}

#[tokio::test]
async fn test_static_target_not_found() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![])?;
    assert!(ws.run("//nowhere:thing").await.is_err());
    Ok(())
}

#[tokio::test]
async fn test_static_multiple_packages_independent() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        bash("//pkg1:t", "printf 'pkg1' > $OUT", "out.txt"),
        bash("//pkg2:t", "printf 'pkg2' > $OUT", "out.txt"),
    ])?;

    let r1 = ws.run("//pkg1:t").await?;
    let r1_content = common::artifact_string(&r1);

    let r2 = ws.run("//pkg2:t").await?;
    let r2_content = common::artifact_string(&r2);

    assert!(r1_content.contains("pkg1"), "pkg1: {r1_content:?}");
    assert!(r2_content.contains("pkg2"), "pkg2: {r2_content:?}");
    Ok(())
}

#[tokio::test]
async fn test_static_packages_isolated() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        bash("//sib/a:t", "printf 'A' > $OUT", "out.txt"),
        bash("//sib/b:t", "printf 'B' > $OUT", "out.txt"),
    ])?;

    let ra = ws.run("//sib/a:t").await?;
    let ra_content = common::artifact_string(&ra);

    let rb = ws.run("//sib/b:t").await?;
    let rb_content = common::artifact_string(&rb);

    assert!(ra_content.contains("A"), "sib/a: {ra_content:?}");
    assert!(rb_content.contains("B"), "sib/b: {rb_content:?}");
    Ok(())
}

#[tokio::test]
async fn test_static_matcher_package_all_targets() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        bash("//mypkg:x", "printf 'X' > $OUT", "x.txt"),
        bash("//mypkg:y", "printf 'Y' > $OUT", "y.txt"),
        bash("//other:z", "printf 'Z' > $OUT", "z.txt"),
    ])?;

    let e = ws.engine.clone();
    let rs = e.new_state();
    let matcher = Matcher::Package(PkgBuf::from("mypkg"));
    let results = e.result(rs, &matcher, &ResultOptions::default()).await?;

    assert_eq!(results.len(), 2, "expected 2 targets in //mypkg");
    Ok(())
}

#[tokio::test]
async fn test_static_cached_run() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![bash(
        "//cached:t",
        "printf 'ok' > $OUT",
        "out.txt",
    )])?;

    let r1 = ws.run("//cached:t").await?;
    let r1_content = common::artifact_string(&r1);

    let r2 = ws.run("//cached:t").await?;
    let r2_content = common::artifact_string(&r2);

    assert!(r1_content.contains("ok"));
    assert!(r2_content.contains("ok"));
    Ok(())
}

fn artifact_string(result: &rheph::engine::EResult) -> String {
    common::artifact_string(result)
}
