mod common;

use common::Workspace;
use rheph::loosespecparser::TargetSpecValue;
// ── Discovery ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_nested_package_discovered() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("a/b/c", r#"target(name = "deep", driver = "bash", run = "true")"#);

    let spec = ws.get_spec("//a/b/c:deep").await?;
    assert_eq!(spec.addr.package.as_str(), "a/b/c");
    Ok(())
}

#[tokio::test]
async fn test_missing_build_file_not_found() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_file("nopkg/some_file.txt", "hello");

    assert!(ws.get_spec("//nopkg:anything").await.is_err());
    Ok(())
}

// ── Spec assertions ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_spec_driver_field() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("pkg", r#"target(name = "t", driver = "bash", run = "true")"#);

    let spec = ws.get_spec("//pkg:t").await?;
    assert_eq!(spec.driver, "bash");
    Ok(())
}

#[tokio::test]
async fn test_spec_run_string() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("pkg", r#"target(name = "t", driver = "bash", run = "echo hello")"#);

    let spec = ws.get_spec("//pkg:t").await?;
    match spec.config.get("run") {
        Some(TargetSpecValue::String(s)) => assert_eq!(s, "echo hello"),
        other => panic!("expected String, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn test_spec_run_list() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "pkg",
        r#"target(name = "t", driver = "bash", run = ["step1", "step2"])"#,
    );

    let spec = ws.get_spec("//pkg:t").await?;
    match spec.config.get("run") {
        Some(TargetSpecValue::List(items)) => {
            assert_eq!(items.len(), 2);
            assert!(matches!(&items[0], TargetSpecValue::String(s) if s == "step1"));
            assert!(matches!(&items[1], TargetSpecValue::String(s) if s == "step2"));
        }
        other => panic!("expected List, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn test_spec_out_field() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "pkg",
        r#"target(name = "t", driver = "bash", run = "true", out = "result.txt")"#,
    );

    let spec = ws.get_spec("//pkg:t").await?;
    match spec.config.get("out") {
        Some(TargetSpecValue::String(s)) => assert_eq!(s, "result.txt"),
        other => panic!("expected String, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn test_spec_labels() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "pkg",
        r#"target(name = "t", driver = "bash", run = "true", labels = ["//team:foo", "//ci:lint"])"#,
    );

    let spec = ws.get_spec("//pkg:t").await?;
    assert!(spec.labels.contains(&"//team:foo".to_string()), "labels: {:?}", spec.labels);
    assert!(spec.labels.contains(&"//ci:lint".to_string()), "labels: {:?}", spec.labels);
    Ok(())
}

#[tokio::test]
async fn test_spec_addr_matches_request() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("mypkg", r#"target(name = "mytgt", driver = "bash", run = "true")"#);

    let spec = ws.get_spec("//mypkg:mytgt").await?;
    assert_eq!(spec.addr.package.as_str(), "mypkg");
    assert_eq!(spec.addr.name, "mytgt");
    Ok(())
}

#[tokio::test]
async fn test_spec_multiple_targets_in_one_file() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "multi",
        r#"
target(name = "a", driver = "bash", run = "true")
target(name = "b", driver = "exec", run = "true")
"#,
    );

    let sa = ws.get_spec("//multi:a").await?;
    let sb = ws.get_spec("//multi:b").await?;

    assert_eq!(sa.driver, "bash");
    assert_eq!(sb.driver, "exec");
    Ok(())
}

#[tokio::test]
async fn test_spec_unknown_target_in_existing_package() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("pkg", r#"target(name = "real", driver = "bash", run = "true")"#);

    assert!(ws.get_spec("//pkg:ghost").await.is_err());
    Ok(())
}
