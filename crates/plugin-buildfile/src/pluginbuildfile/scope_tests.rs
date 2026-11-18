//! The buildfile provider's caches live for one cache scope (I1, I2, I4):
//! a new scope reads the workspace as it is now, one scope answers from one
//! snapshot, and an ended scope is never recreated by a late call or write.

use crate::pluginbuildfile::provider::Provider;
use hcore::hasync::StdCancellationToken;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{
    GetRequest, ListPackagesRequest, ListRequest, NoopExecutor, ProbeRequest, Provider as EProvider,
};
use hplugin::scope::{ScopeEnded, ScopeEndedAck, ScopeId};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn scope(n: u64) -> Option<ScopeId> {
    ScopeId::new(n)
}

fn provider(root: &Path) -> Provider {
    Provider {
        root: root.to_path_buf(),
        build_file_patterns: vec![glob::Pattern::new("BUILD").unwrap()],
        ..Provider::default()
    }
}

async fn packages(p: &Provider, s: Option<ScopeId>) -> anyhow::Result<Vec<String>> {
    let ctoken = StdCancellationToken::new();
    let req = ListPackagesRequest::new("test", PkgBuf::from("")).with_decoded_scope(s);
    p.list_packages(req, &ctoken)
        .await?
        .map(|r| r.map(|r| r.pkg.to_string()))
        .collect()
}

async fn targets(p: &Provider, pkg: &str, s: Option<ScopeId>) -> anyhow::Result<Vec<String>> {
    let ctoken = StdCancellationToken::new();
    let req = ListRequest::new("test", PkgBuf::from(pkg), vec![], Arc::new(NoopExecutor))
        .with_decoded_scope(s);
    p.list(req, &ctoken)
        .await?
        .map(|r| r.map(|r| r.addr.name.clone()))
        .collect()
}

async fn get(
    p: &Provider,
    pkg: &str,
    name: &str,
    s: Option<ScopeId>,
) -> Result<hplugin::provider::GetResponse, hplugin::provider::GetError> {
    let ctoken = StdCancellationToken::new();
    let addr = Addr::new(PkgBuf::from(pkg), name.to_string(), Default::default());
    let req = GetRequest::new("test", addr, vec![], Arc::new(NoopExecutor)).with_decoded_scope(s);
    p.get(req, &ctoken).await
}

async fn probe(p: &Provider, pkg: &str, s: Option<ScopeId>) -> anyhow::Result<usize> {
    let ctoken = StdCancellationToken::new();
    let req = ProbeRequest::new("test", PkgBuf::from(pkg)).with_decoded_scope(s);
    Ok(p.probe(req, &ctoken).await?.states.len())
}

async fn end(p: &Provider, n: u64) {
    let ack = p
        .scope_ended(ScopeEnded::new(ScopeId::new(n).unwrap(), 1))
        .await
        .expect("scope_ended");
    assert_eq!(ack, ScopeEndedAck::Released);
}

/// The `pkgs` list a `heph.core.packages()` call put in a target's config.
fn pkgs_config(spec: &hplugin::provider::GetResponse) -> Vec<String> {
    match spec.target_spec.config.get("pkgs").expect("pkgs config") {
        hcore::htvalue::Value::List(v) => v
            .iter()
            .map(|e| match e {
                hcore::htvalue::Value::String(s) => s.clone(),
                other => panic!("expected a string package, got {other:?}"),
            })
            .collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

/// A package created after a scope listed the tree is seen by the next scope
/// — and not by the one that already listed it.
#[tokio::test]
async fn next_scope_lists_a_new_package() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("BUILD"), "").unwrap();
    let p = provider(root);

    assert_eq!(packages(&p, scope(1)).await.unwrap(), vec![""]);
    fs::create_dir_all(root.join("a")).unwrap();
    fs::write(root.join("a/BUILD"), "").unwrap();
    assert_eq!(
        packages(&p, scope(1)).await.unwrap(),
        vec![""],
        "one scope, one snapshot"
    );

    end(&p, 1).await;
    assert_eq!(packages(&p, scope(2)).await.unwrap(), vec!["", "a"]);
}

/// A BUILD file edited between scopes: the next scope lists the added target
/// and no longer lists the removed one, through `list` and `get` alike.
#[tokio::test]
async fn next_scope_lists_added_and_drops_removed_targets() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("p")).unwrap();
    let build = root.join("p/BUILD");
    fs::write(
        &build,
        "target(name = \"old\", driver = \"d\")\ntarget(name = \"kept\", driver = \"d\")\n",
    )
    .unwrap();
    let p = provider(root);

    assert_eq!(
        targets(&p, "p", scope(1)).await.unwrap(),
        vec!["old", "kept"]
    );
    fs::write(
        &build,
        "target(name = \"kept\", driver = \"d\")\ntarget(name = \"new\", driver = \"d\")\n",
    )
    .unwrap();
    assert!(
        get(&p, "p", "new", scope(1)).await.is_err(),
        "scope 1 answers from the BUILD file it read",
    );

    end(&p, 1).await;
    assert_eq!(
        targets(&p, "p", scope(2)).await.unwrap(),
        vec!["kept", "new"]
    );
    assert!(get(&p, "p", "new", scope(2)).await.is_ok());
    assert!(matches!(
        get(&p, "p", "old", scope(2)).await,
        Err(hplugin::provider::GetError::NotFound)
    ));
}

/// A package that failed to evaluate stays failed for the rest of its scope
/// (sticky errors), and is evaluated again — successfully, once fixed — in the
/// next.
#[tokio::test]
async fn next_scope_retries_a_failed_package() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("p")).unwrap();
    let build = root.join("p/BUILD");
    fs::write(&build, "target(name = \"t\", driver = \n").unwrap();
    let p = provider(root);

    assert!(targets(&p, "p", scope(1)).await.is_err());
    fs::write(&build, "target(name = \"t\", driver = \"d\")\n").unwrap();
    assert!(
        targets(&p, "p", scope(1)).await.is_err(),
        "an evaluation error is sticky within its scope",
    );

    end(&p, 1).await;
    assert_eq!(targets(&p, "p", scope(2)).await.unwrap(), vec!["t"]);
}

/// Within one scope every answer comes from one snapshot: `list_packages`,
/// `list`, `get`, `probe` and a `heph.core.packages()` evaluated *after* the
/// tree changed all agree with what the scope first read. A concurrent second
/// scope, still open beside it, sees the changed tree.
#[tokio::test]
async fn a_scope_answers_from_one_snapshot() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("w")).unwrap();
    fs::write(
        root.join("w/BUILD"),
        "target(name = \"t\", driver = \"d\", pkgs = heph.core.packages(\"//...\"))\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("s")).unwrap();
    fs::write(
        root.join("s/BUILD"),
        "provider_state(provider = \"x\")\ntarget(name = \"t\", driver = \"d\")\n",
    )
    .unwrap();
    let p = provider(root);

    assert_eq!(packages(&p, scope(1)).await.unwrap(), vec!["", "s", "w"]);
    assert_eq!(probe(&p, "s", scope(1)).await.unwrap(), 1);

    // The tree changes mid-scope: a package appears, a state and a target go.
    fs::create_dir_all(root.join("late")).unwrap();
    fs::write(root.join("late/BUILD"), "").unwrap();
    fs::write(root.join("s/BUILD"), "").unwrap();

    assert_eq!(packages(&p, scope(1)).await.unwrap(), vec!["", "s", "w"]);
    // `w` is first evaluated now, after the change: its `heph.core.packages()`
    // reads scope 1's list, not a fresh walk.
    let w = get(&p, "w", "t", scope(1)).await.unwrap();
    assert_eq!(pkgs_config(&w), vec!["", "s", "w"]);
    assert_eq!(probe(&p, "s", scope(1)).await.unwrap(), 1);
    assert_eq!(targets(&p, "s", scope(1)).await.unwrap(), vec!["t"]);

    // Scope 2 is open while scope 1 still is: it reads the tree as it is.
    assert_eq!(
        packages(&p, scope(2)).await.unwrap(),
        vec!["", "late", "s", "w"]
    );
    let w2 = get(&p, "w", "t", scope(2)).await.unwrap();
    assert_eq!(pkgs_config(&w2), vec!["", "late", "s", "w"]);
    assert_eq!(probe(&p, "s", scope(2)).await.unwrap(), 0);
    assert!(targets(&p, "s", scope(2)).await.unwrap().is_empty());
}

/// A call that resolved its bucket before the scope ended may still finish
/// after it; what it writes dies with the detached bucket. The scope is not
/// recreated: a later call for it is answered uncached, from the tree as it is.
#[tokio::test]
async fn late_write_after_scope_end_is_discarded() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let build = root.join("BUILD");
    fs::write(&build, "target(name = \"a\", driver = \"d\")\n").unwrap();
    let p = provider(root);

    // An entry point resolved scope 1's bucket, then the scope ended before
    // its evaluation ran.
    let straggler = p.caches(scope(1));
    end(&p, 1).await;
    let late = p.run_pkg(&straggler, "").await.unwrap();
    assert_eq!(late.targets.len(), 1);
    assert_eq!(
        p.scopes.live_scopes(),
        0,
        "the late write recreated nothing"
    );

    fs::write(
        &build,
        "target(name = \"a\", driver = \"d\")\ntarget(name = \"b\", driver = \"d\")\n",
    )
    .unwrap();
    assert_eq!(
        targets(&p, "", scope(1)).await.unwrap(),
        vec!["a", "b"],
        "a call for an ended scope must not be served the late write",
    );
    assert_eq!(p.scopes.live_scopes(), 0, "nor recreate the bucket");
}

/// A file `load()`ed by two packages is evaluated once per scope between
/// them, and once more in the next scope — which therefore sees its edit.
#[tokio::test]
async fn shared_load_evaluated_once_per_scope_and_again_in_the_next() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    for pkg in ["a", "b"] {
        fs::create_dir_all(root.join(pkg)).unwrap();
        fs::write(
            root.join(pkg).join("BUILD"),
            "load(\"//shared.BUILD\", \"N\")\ntarget(name = \"t\", driver = \"d\", n = N)\n",
        )
        .unwrap();
    }
    let p = provider(root);

    for n in [1, 2] {
        let version = format!("v{n}");
        fs::write(root.join("shared.BUILD"), format!("N = \"{version}\"\n")).unwrap();
        let s = scope(n);
        let caches = p.caches(s);
        let (a, b) = tokio::join!(get(&p, "a", "t", s), get(&p, "b", "t", s));
        for spec in [a.unwrap(), b.unwrap()] {
            assert_eq!(
                spec.target_spec.config.get("n"),
                Some(&hcore::htvalue::Value::String(version.clone())),
                "scope {n} reads the shared file as it is",
            );
        }
        // `a/BUILD`, `b/BUILD` and `shared.BUILD`: one evaluation apiece.
        assert_eq!(caches.loads.started(), 3, "scope {n}");
        end(&p, n).await;
    }
}

/// The buildfile half of `sticky_error_does_not_leak_to_next_scope`: an
/// evaluation error cached by `probe` in one scope does not answer `get` in the
/// next. And a failed package *walk* is not sticky at all: it is retried
/// within the same scope.
#[tokio::test]
async fn sticky_error_does_not_leak_to_next_scope() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("p")).unwrap();
    let build = root.join("p/BUILD");
    fs::write(&build, "X = undefined_symbol\n").unwrap();
    let p = provider(root);

    assert!(probe(&p, "p", scope(1)).await.is_err());
    fs::write(&build, "target(name = \"t\", driver = \"d\")\n").unwrap();
    assert!(
        get(&p, "p", "t", scope(1)).await.is_err(),
        "sticky in scope 1"
    );
    end(&p, 1).await;
    assert!(
        get(&p, "p", "t", scope(2)).await.is_ok(),
        "fresh in scope 2"
    );

    // A walk that fails (an unreadable directory) is retried within the scope.
    let locked = root.join("locked");
    fs::create_dir_all(&locked).unwrap();
    set_mode(&locked, 0o000);
    if fs::read_dir(&locked).is_ok() {
        // Running as root: permissions do not stop the walk.
        set_mode(&locked, 0o755);
        eprintln!("SKIP: an unreadable directory is readable here (root?)");
        return;
    }
    let failed = packages(&p, scope(3)).await;
    set_mode(&locked, 0o755);
    assert!(failed.is_err(), "the walk must fail on an unreadable dir");
    assert_eq!(
        packages(&p, scope(3)).await.unwrap(),
        vec!["", "p"],
        "a failed walk is retried within its scope",
    );
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
