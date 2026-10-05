//! Listed facts: what a provider's `list` says about each target — labels,
//! driver, `has_codegen` — decides a query without `get` where it can. A listed
//! No is final on every path (selection and the query-target walk); a listed
//! Yes is a candidate whose existence is confirmed as before. `heph validate`
//! holds the facts to the targets they describe.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use futures::TryStreamExt;
use heph::engine::discovery::Stage;
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::fault_provider::{FactsFn, FaultProvider, Faults, GetLog};
use heph::engine::listed::ListedFactsTrust;
use heph::engine::request_state::RequestState;
use heph::engine::{Discovery, Gaps, OutputMatcher, ResultOptions, StatesOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::{ListedFacts, Matcher};
use heph::htpkg::PkgBuf;
use heph::htvalue::Value;
use heph::pluginexec;
use heph::pluginquery::PACKAGE;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const TRUST: ListedFactsTrust = ListedFactsTrust::Trust;
const IGNORE: ListedFactsTrust = ListedFactsTrust::Ignore;

fn bash(addr: &str, labels: &[&str]) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some("echo ok > $OUT".to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        labels: strings(labels),
        ..Default::default()
    }
}

fn codegen(addr: &str) -> Target {
    Target {
        codegen: Some("copy".to_string()),
        ..bash(addr, &[])
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|l| (*l).to_string()).collect()
}

fn label(l: &str) -> Matcher {
    Matcher::Label(l.to_string())
}

fn driver(d: &str) -> Matcher {
    Matcher::Driver(d.to_string())
}

fn everything() -> Matcher {
    Matcher::PackagePrefix(PkgBuf::from(""))
}

/// The C1 package: `lib` (no labels), `test` (`test`, `go-test`) and `format`
/// (`format`, `fix`), every one a `bash` target.
fn c1_targets() -> Vec<Target> {
    vec![
        bash("//p:lib", &[]),
        bash("//p:test", &["test", "go-test"]),
        bash("//p:format", &["format", "fix"]),
    ]
}

fn facts_fn(f: impl Fn(&Addr) -> Option<ListedFacts> + Send + Sync + 'static) -> Option<FactsFn> {
    Some(FactsFn::new(move |a, _| f(a)))
}

fn builder(providers: Vec<FaultProvider>) -> anyhow::Result<htestkit::Workspace> {
    let mut b = htestkit::WorkspaceBuilder::new()?;
    for p in providers {
        b = b.with_provider(move |_| Box::new(p));
    }
    b.with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()
}

fn workspace(
    targets: Vec<Target>,
    faults: Faults,
) -> anyhow::Result<(htestkit::Workspace, Arc<GetLog>)> {
    let provider = FaultProvider::new(targets, faults)?;
    let gets = provider.gets();
    Ok((builder(vec![provider])?, gets))
}

fn got(gets: &GetLog) -> Vec<String> {
    let mut v: Vec<String> = gets.addrs().iter().map(Addr::format).collect();
    v.sort();
    v
}

async fn select(
    ws: &htestkit::Workspace,
    rs: Arc<RequestState>,
    m: &Matcher,
) -> anyhow::Result<Vec<String>> {
    let addrs: Vec<Addr> = Arc::clone(&ws.engine)
        .query(rs, m, Discovery::Complete)
        .try_collect()
        .await?;
    Ok(addrs.iter().map(Addr::format).collect())
}

fn query_addr(expr: &str) -> String {
    format!("//{PACKAGE}:q@expr={expr}")
}

/// The dep list a query target resolves to, in order.
async fn deps(
    ws: &htestkit::Workspace,
    rs: Arc<RequestState>,
    expr: &str,
) -> anyhow::Result<Vec<String>> {
    deps_of(ws, rs, &query_addr(expr)).await
}

/// [`deps`] of a query target given by its full addr.
async fn deps_of(
    ws: &htestkit::Workspace,
    rs: Arc<RequestState>,
    query: &str,
) -> anyhow::Result<Vec<String>> {
    let spec = Arc::clone(&ws.engine)
        .get_spec(rs, &parse_addr(query)?)
        .await?;
    match spec.config.get("deps") {
        Some(Value::List(l)) => l
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                other => anyhow::bail!("expected a string dep, got {other:?}"),
            })
            .collect(),
        other => anyhow::bail!("expected a deps list, got {other:?}"),
    }
}

fn trust(ws: &htestkit::Workspace, t: ListedFactsTrust) -> Arc<RequestState> {
    ws.engine.new_state_with_listed_facts_trust(t)
}

/// `heph r <matcher>`, returning the batch and every event it emitted.
async fn run(
    ws: &htestkit::Workspace,
    m: &Matcher,
    opts: &ResultOptions,
) -> (anyhow::Result<heph::engine::BatchResult>, Vec<BuildEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = ws.engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(&ws.engine)
        .result(rs.clone(), m, OutputMatcher::All, opts)
        .await;
    drop(rs);
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (res, events)
}

fn built(events: &[BuildEvent]) -> Vec<String> {
    let mut addrs: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr, error: None, ..
            } => Some(addr.clone()),
            _ => None,
        })
        .collect();
    addrs.sort();
    addrs
}

async fn mismatches(ws: &htestkit::Workspace) -> anyhow::Result<Vec<String>> {
    let mut found: Vec<String> = Arc::clone(&ws.engine)
        .listed_fact_mismatches(ws.engine.new_state(), &everything(), Discovery::Complete)
        .await?
        .iter()
        .map(ToString::to_string)
        .collect();
    found.sort();
    Ok(found)
}

/// C1, C2: listed labels decide, and nothing is resolved.
#[tokio::test]
async fn listed_labels_decide_without_get() -> anyhow::Result<()> {
    let (ws, gets) = workspace(c1_targets(), Faults::default())?;
    assert_eq!(
        select(&ws, trust(&ws, TRUST), &label("test")).await?,
        vec!["//p:test"]
    );
    assert!(
        select(&ws, trust(&ws, TRUST), &label("deploy"))
            .await?
            .is_empty()
    );
    assert!(got(&gets).is_empty(), "resolved {:?}", got(&gets));
    Ok(())
}

/// C3: only the entry whose labels are unknown is resolved.
#[tokio::test]
async fn unknown_field_resolves_spec() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        c1_targets(),
        Faults {
            listed_facts: facts_fn(|a| {
                (a.name == "test").then(|| ListedFacts::default().with_driver("bash"))
            }),
            ..Default::default()
        },
    )?;
    assert_eq!(
        select(&ws, trust(&ws, TRUST), &label("test")).await?,
        vec!["//p:test"]
    );
    assert_eq!(got(&gets), vec!["//p:test"]);
    Ok(())
}

/// C4: a provider that lists no facts — `addr_only`, or a plugin older than
/// ABI 0.12 — gets exactly the results and `get`s of a walk that ignores
/// facts.
#[tokio::test]
async fn addr_only_listing_behaves_as_today() -> anyhow::Result<()> {
    let faults = || Faults {
        facts_unknown: true,
        ..Default::default()
    };
    let (ws, gets) = workspace(c1_targets(), faults())?;
    let trusted = select(&ws, trust(&ws, TRUST), &label("test")).await?;
    let trusted_gets = got(&gets);
    let (ws, gets) = workspace(c1_targets(), faults())?;
    let ignored = select(&ws, trust(&ws, IGNORE), &label("test")).await?;
    assert_eq!(trusted, vec!["//p:test"]);
    assert_eq!(trusted, ignored);
    assert_eq!(trusted_gets, got(&gets));
    assert_eq!(trusted_gets, vec!["//p:format", "//p:lib", "//p:test"]);
    Ok(())
}

/// C5, C27: the buildfile provider lists every target's driver — the one its
/// `get` returns, `defaultDriver` included — so `driver("credential")` resolves
/// no BUILD spec.
#[tokio::test]
async fn buildfile_listing_carries_driver() -> anyhow::Result<()> {
    use heph::engine::provider::{ListRequest, NoopExecutor};
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(|init| {
            let mut p =
                heph::pluginbuildfile::Provider::new(init.root.to_path_buf(), init.runtime.clone());
            p.default_driver = Some("bash".to_string());
            Box::new(p)
        })
        .build()?;
    ws.write_build_file(
        "p",
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt", labels = ["x"])
target(name = "b", driver = "exec", run = ["true"])
target(name = "c", run = "echo c")
"#,
    );
    let tok = heph::hasync::StdCancellationToken::new();
    let listed: Vec<_> = ws.engine.providers_by_name["buildfile"]
        .provider
        .list(
            ListRequest {
                request_id: "t".to_string(),
                package: PkgBuf::from("p"),
                states: vec![],
                executor: Arc::new(NoopExecutor),
            },
            &tok,
        )
        .await?
        .collect::<anyhow::Result<_>>()?;
    let facts: Vec<String> = listed
        .iter()
        .map(|r| {
            format!(
                "{} {:?} {:?} {:?}",
                r.addr.name,
                r.facts.driver(),
                r.facts.labels(),
                r.facts.has_codegen(),
            )
        })
        .collect();
    assert_eq!(
        facts,
        vec![
            r#"a Some("bash") Some(["x"]) None"#,
            r#"b Some("exec") Some([]) None"#,
            r#"c Some("bash") Some([]) None"#,
        ],
        "every target's driver is listed; has_codegen stays unknown (R4)"
    );
    for r in &listed {
        assert_eq!(
            driver("credential").matches_listed(&r.addr, &r.facts),
            heph::htmatcher::MatchResult::MatchNo
        );
    }
    Ok(())
}

/// C7: only the entry whose driver is unknown is resolved.
#[tokio::test]
async fn unknown_driver_entry_resolves_spec() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        c1_targets(),
        Faults {
            listed_facts: facts_fn(|a| {
                (a.name == "lib").then(|| ListedFacts::default().with_labels(Vec::<String>::new()))
            }),
            ..Default::default()
        },
    )?;
    assert!(
        select(&ws, trust(&ws, TRUST), &driver("credential"))
            .await?
            .is_empty()
    );
    assert_eq!(got(&gets), vec!["//p:lib"]);
    Ok(())
}

/// C16, C17: an explicit addr is decided by the addr, and a target that is
/// gettable but never listed still resolves and builds.
#[tokio::test]
async fn explicit_and_unlisted_addrs_resolve_as_today() -> anyhow::Result<()> {
    let mut targets = c1_targets();
    targets.push(bash("//p:hidden", &["test"]));
    let (ws, _) = workspace(
        targets,
        Faults {
            unlisted: vec![parse_addr("//p:hidden")?],
            // Lies everywhere: an addr matcher must not read them.
            listed_facts: facts_fn(|_| Some(ListedFacts::default().with_labels(["lie"]))),
            ..Default::default()
        },
    )?;
    for addr in ["//p:test", "//p:hidden"] {
        let m = Matcher::Addr(parse_addr(addr)?);
        assert_eq!(
            select(&ws, trust(&ws, TRUST), &m).await?,
            select(&ws, trust(&ws, IGNORE), &m).await?,
            "{addr}: decided by the addr alone"
        );
    }
    let (res, events) = run(
        &ws,
        &Matcher::Addr(parse_addr("//p:test")?),
        &ResultOptions::default(),
    )
    .await;
    res?;
    assert_eq!(built(&events), vec!["//p:test"]);
    // Named outright, a target no listing mentions still resolves and builds.
    drop(ws.run("//p:hidden").await?);
    Ok(())
}

/// C18: a matcher with no fact-reading arm reads no fact — a lie everywhere
/// changes nothing, and nothing is resolved or counted as decided.
#[tokio::test]
async fn fact_free_matcher_reads_no_facts() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        c1_targets(),
        Faults {
            listed_facts: facts_fn(|_| {
                Some(
                    ListedFacts::default()
                        .with_labels(["lie"])
                        .with_driver("lie"),
                )
            }),
            ..Default::default()
        },
    )?;
    let rs = trust(&ws, TRUST);
    for m in [everything(), Matcher::Package(PkgBuf::from("p"))] {
        assert_eq!(
            select(&ws, Arc::clone(&rs), &m).await?,
            vec!["//p:lib", "//p:test", "//p:format"]
        );
    }
    assert!(got(&gets).is_empty());
    assert_eq!(rs.listed_decided(), 0);
    Ok(())
}

/// C19, C20: two listers that disagree on a field, or one that does not know
/// it, leave it unknown — the spec decides.
#[tokio::test]
async fn merge_unknown_on_disagreement() -> anyhow::Result<()> {
    // C19: A lists `x` with `go_compile`, B (which resolves it) with `bash`.
    let a = FaultProvider::new(
        vec![],
        Faults {
            name: Some("a"),
            vanished: vec![(parse_addr("//p:x")?, vec![])],
            listed_facts: facts_fn(|_| Some(ListedFacts::default().with_driver("go_compile"))),
            ..Default::default()
        },
    )?;
    let b = FaultProvider::new(
        vec![bash("//p:x", &["test"])],
        Faults {
            name: Some("b"),
            ..Default::default()
        },
    )?;
    let b_gets = b.gets();
    let ws = builder(vec![a, b])?;
    assert!(
        select(&ws, trust(&ws, TRUST), &driver("go_compile"))
            .await?
            .is_empty()
    );
    assert_eq!(got(&b_gets), vec!["//p:x"], "the spec decided");

    // C20: A knows `test`'s labels, B lists it with every fact unknown.
    let a = FaultProvider::new(
        vec![],
        Faults {
            name: Some("a"),
            vanished: vec![(parse_addr("//p:test")?, strings(&["test"]))],
            ..Default::default()
        },
    )?;
    let b = FaultProvider::new(
        vec![bash("//p:test", &["other"])],
        Faults {
            name: Some("b"),
            facts_unknown: true,
            ..Default::default()
        },
    )?;
    let b_gets = b.gets();
    let ws = builder(vec![a, b])?;
    assert!(
        select(&ws, trust(&ws, TRUST), &label("test"))
            .await?
            .is_empty(),
        "A's listed Yes must not decide alone"
    );
    assert_eq!(got(&b_gets), vec!["//p:test"]);
    Ok(())
}

/// C21, C30: under KeepGoing, a co-lister's failed `list` of the package — or
/// failed `list_packages` covering it — leaves every fact there unknown: the
/// spec decides, and the gap is recorded.
#[tokio::test]
async fn failed_colister_makes_package_facts_unknown() -> anyhow::Result<()> {
    for (what, b_faults) in [
        (
            "list",
            Faults {
                name: Some("b"),
                fail_list: vec!["p".to_string()],
                ..Default::default()
            },
        ),
        (
            "list_packages",
            Faults {
                name: Some("b"),
                fail_list_packages: true,
                ..Default::default()
            },
        ),
    ] {
        let a = FaultProvider::new(
            c1_targets(),
            Faults {
                name: Some("a"),
                ..Default::default()
            },
        )?;
        let a_gets = a.gets();
        let b = FaultProvider::new(vec![bash("//p:only_b", &[])], b_faults)?;
        let ws = builder(vec![a, b])?;
        let gaps = Gaps::new("label(test)");
        let addrs: Vec<Addr> = Arc::clone(&ws.engine)
            .query(
                trust(&ws, TRUST),
                &label("test"),
                Discovery::KeepGoing(gaps.clone()),
            )
            .try_collect()
            .await?;
        assert_eq!(addrs, vec![parse_addr("//p:test")?], "{what}");
        let resolved = got(&a_gets);
        for want in ["//p:format", "//p:lib", "//p:test"] {
            assert!(
                resolved.iter().any(|a| a == want),
                "{what}: {want} did not go to the spec: {resolved:?}"
            );
        }
        assert!(!gaps.is_empty(), "{what}: the gap is recorded");
    }
    Ok(())
}

/// C22: a BUILD `query()` drops a listed No without `get`, and confirms a
/// listed Yes with one `get_spec`; the dep set is the resolved-truth one.
#[tokio::test]
async fn executor_query_uses_listed_facts() -> anyhow::Result<()> {
    let (ws, gets) = workspace(c1_targets(), Faults::default())?;
    let trusted = deps(&ws, trust(&ws, TRUST), "label(test)").await?;
    assert_eq!(trusted, vec!["//p:test"]);
    assert_eq!(got(&gets), vec!["//p:test"], "one get_spec, for the Yes");

    let (ws, gets) = workspace(c1_targets(), Faults::default())?;
    assert_eq!(deps(&ws, trust(&ws, IGNORE), "label(test)").await?, trusted);
    assert_eq!(got(&gets), vec!["//p:format", "//p:lib", "//p:test"]);
    Ok(())
}

/// C22, Invariant 1 (R3): with honest facts, trusting them changes no byte of
/// the dep list, the def hash or `hashin` — the candidate sequence,
/// duplicates and order included, is the one `Ignore` builds.
#[tokio::test]
async fn executor_query_trusted_equals_untrusted() -> anyhow::Result<()> {
    // Two listers of `test` (the second a duplicate listing), spread over two
    // packages, so order and duplicates are in play.
    let a = FaultProvider::new(
        vec![
            bash("//p:test", &["test"]),
            bash("//p:lib", &[]),
            bash("//q:test", &["test"]),
        ],
        Faults {
            name: Some("a"),
            ..Default::default()
        },
    )?;
    let b = FaultProvider::new(
        vec![],
        Faults {
            name: Some("b"),
            vanished: vec![(parse_addr("//q:test")?, strings(&["test"]))],
            ..Default::default()
        },
    )?;
    let ws = builder(vec![a, b])?;
    let q = parse_addr(&query_addr("label(test)"))?;
    let mut seen = Vec::new();
    for t in [TRUST, IGNORE] {
        let rs = trust(&ws, t);
        let deps = deps(&ws, Arc::clone(&rs), "label(test)").await?;
        let def = Arc::clone(&ws.engine).get_def(Arc::clone(&rs), &q).await?;
        let hashin = Arc::clone(&ws.engine).meta(rs, &q).await?.hashin;
        seen.push((deps, def.target_def.hash.clone(), hashin));
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0].0, vec!["//p:test", "//q:test", "//q:test"]);
    Ok(())
}

/// C23: `Ignore` — what the CLI sets from `HEPH_NO_LISTED_FACTS=1` — makes the
/// walk resolve every candidate a fact would have decided, as before.
#[tokio::test]
async fn kill_switch_ignores_listed_facts() -> anyhow::Result<()> {
    assert_eq!(ListedFactsTrust::from_kill_switch(true), IGNORE);
    assert_eq!(ListedFactsTrust::from_kill_switch(false), TRUST);

    let (ws, gets) = workspace(c1_targets(), Faults::default())?;
    let rs = trust(&ws, IGNORE);
    assert_eq!(
        select(&ws, Arc::clone(&rs), &label("test")).await?,
        vec!["//p:test"]
    );
    assert_eq!(got(&gets), vec!["//p:format", "//p:lib", "//p:test"]);
    assert_eq!(rs.listed_decided(), 0);

    let (ws, _) = workspace(c1_targets(), Faults::default())?;
    let rs = trust(&ws, TRUST);
    select(&ws, Arc::clone(&rs), &label("test")).await?;
    assert_eq!(rs.listed_decided(), 3);
    Ok(())
}

/// C24: facts are per entry under the chain `list` received: a state under
/// `//foo` changes only the entries listed there.
#[tokio::test]
async fn state_changes_listed_labels_per_target() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        vec![
            bash("//foo/x:test", &["test"]),
            bash("//bar:test", &["test"]),
        ],
        Faults {
            name: Some("go"),
            states: vec![(
                "foo".to_string(),
                HashMap::from([("custom".to_string(), Value::Bool(true))]),
            )],
            listed_facts: Some(FactsFn::new(|_, states| {
                let custom = states.iter().any(|s| s.state.contains_key("custom"));
                let mut labels = vec!["test"];
                if custom {
                    labels.push("custom");
                }
                Some(ListedFacts::default().with_labels(labels))
            })),
            ..Default::default()
        },
    )?;
    assert_eq!(
        select(&ws, trust(&ws, TRUST), &label("custom")).await?,
        vec!["//foo/x:test"]
    );
    assert!(got(&gets).is_empty());
    Ok(())
}

/// C26: a listed Yes `get` declines is dropped by `heph r` and by the
/// query-target walk.
#[tokio::test]
async fn listed_yes_not_found_dropped_by_run() -> anyhow::Result<()> {
    let faults = || -> anyhow::Result<Faults> {
        Ok(Faults {
            vanished: vec![(parse_addr("//p:ghost")?, strings(&["test"]))],
            ..Default::default()
        })
    };
    let (ws, _) = workspace(vec![bash("//p:real", &["test"])], faults()?)?;
    let (res, events) = run(&ws, &label("test"), &ResultOptions::default()).await;
    let batch = res?;
    assert!(batch.errors.is_empty(), "{:?}", batch.errors);
    assert_eq!(built(&events), vec!["//p:real"]);

    let (ws, gets) = workspace(vec![bash("//p:real", &["test"])], faults()?)?;
    assert_eq!(
        deps(&ws, trust(&ws, TRUST), "label(test)").await?,
        vec!["//p:real"]
    );
    assert_eq!(got(&gets), vec!["//p:ghost", "//p:real"]);
    Ok(())
}

/// C26: `heph q` prints a listed Yes it has not confirmed, as under #474 —
/// frozen, so a change to that is a deliberate one.
#[tokio::test]
async fn heph_q_phantom_test_entries_frozen() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        vec![bash("//p:real", &["test"])],
        Faults {
            vanished: vec![(parse_addr("//p:ghost")?, strings(&["test"]))],
            ..Default::default()
        },
    )?;
    assert_eq!(
        select(&ws, trust(&ws, TRUST), &label("test")).await?,
        vec!["//p:ghost", "//p:real"]
    );
    assert!(got(&gets).is_empty());
    Ok(())
}

/// C29, regression: go lists `test@v=1` (driver `exec`), and a BUILD target
/// named `test` (driver `bash`), whose provider resolves the name whatever its
/// args, answers it. Keyed on the full addr, go's facts alone decided and
/// `driver(exec)` selected the BUILD target. By name, the drivers disagree, the
/// spec decides, and nothing is selected. In either registration order, and
/// `driver(bash)` still selects the BUILD target.
#[tokio::test]
async fn colliding_name_across_providers_merges_by_name() -> anyhow::Result<()> {
    let variant = parse_addr("//p:test@v=1")?;
    for go_first in [true, false] {
        let go = FaultProvider::new(
            vec![],
            Faults {
                name: Some("go"),
                vanished: vec![(variant.clone(), strings(&["test"]))],
                listed_facts: facts_fn(|_| {
                    Some(
                        ListedFacts::default()
                            .with_labels(["test"])
                            .with_driver("exec"),
                    )
                }),
                ..Default::default()
            },
        )?;
        // Resolves `//p:test` whatever the args, like the buildfile provider.
        let buildfile = FaultProvider::new(
            vec![bash("//p:test", &["test"]), bash("//p:test@v=1", &["test"])],
            Faults {
                name: Some("buildfile"),
                unlisted: vec![variant.clone()],
                ..Default::default()
            },
        )?;
        let ws = builder(if go_first {
            vec![go, buildfile]
        } else {
            vec![buildfile, go]
        })?;
        assert!(
            select(&ws, trust(&ws, TRUST), &driver("exec"))
                .await?
                .is_empty(),
            "go's facts must not decide for the BUILD target (go first: {go_first})"
        );
        let by_bash = select(&ws, trust(&ws, TRUST), &driver("bash")).await?;
        assert!(
            by_bash.contains(&"//p:test".to_string()),
            "the bare BUILD `test` is a bash target (go first: {go_first}): {by_bash:?}"
        );
        assert_eq!(
            by_bash,
            select(&ws, trust(&ws, IGNORE), &driver("bash")).await?,
            "go first: {go_first}"
        );
        let (res, events) = run(&ws, &driver("exec"), &ResultOptions::default()).await;
        res?;
        assert!(built(&events).is_empty(), "ran {:?}", built(&events));
    }
    Ok(())
}

/// C29, regression: a listing with a known fact for an addr another provider
/// resolves is reported, not skipped.
#[tokio::test]
async fn validate_reports_lister_not_resolver() -> anyhow::Result<()> {
    let go = FaultProvider::new(
        vec![],
        Faults {
            name: Some("go"),
            vanished: vec![(parse_addr("//p:build")?, strings(&[]))],
            ..Default::default()
        },
    )?;
    let buildfile = FaultProvider::new(
        vec![bash("//p:build", &[])],
        Faults {
            name: Some("buildfile"),
            ..Default::default()
        },
    )?;
    let ws = builder(vec![go, buildfile])?;
    let found = mismatches(&ws).await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains(
            "provider `go` lists //p:build with listed facts, but provider `buildfile` resolves it"
        ),
        "{}",
        found[0]
    );
    Ok(())
}

/// C32: `--force` is decided from the resolved spec, never from listed facts:
/// a dependency whose listing lies No is still forced.
#[tokio::test]
async fn force_selection_ignores_listed_facts() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    let state_file = state.path().join("state");
    std::fs::write(&state_file, "stale")?;
    let dep = Target {
        run: Some(format!("cat '{}' > $OUT", state_file.display())),
        ..bash("//p:dep", &["test"])
    };
    let top = Target {
        run: Some("grep -q fresh $SRC_DEP && echo ok > $OUT".to_string()),
        deps: HashMap::from([("dep".to_string(), vec!["//p:dep".to_string()])]),
        ..bash("//p:top", &["test"])
    };
    let (ws, _) = workspace(
        vec![dep, top],
        Faults {
            listed_labels: vec![(parse_addr("//p:dep")?, vec![])],
            ..Default::default()
        },
    )?;
    drop(ws.run("//p:dep").await?);
    std::fs::write(&state_file, "fresh")?;

    let (res, _) = run(
        &ws,
        &label("test"),
        &ResultOptions {
            force: true,
            ..Default::default()
        },
    )
    .await;
    let batch = res?;
    assert!(
        batch.errors.is_empty(),
        "the stale dep was not forced: {:?}",
        batch.errors
    );
    Ok(())
}

/// C33: a provider whose `list` fails under KeepGoing — what a malformed
/// `ListedFacts` on the wire comes to — is a recorded gap, and the package's
/// facts are unknown.
#[tokio::test]
async fn malformed_listed_facts_fails_list() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        c1_targets(),
        Faults {
            name: Some("a"),
            fail_list: vec!["p".to_string()],
            ..Default::default()
        },
    )?;
    let gaps = Gaps::new("label(test)");
    let addrs: Vec<Addr> = Arc::clone(&ws.engine)
        .query(
            trust(&ws, TRUST),
            &label("test"),
            Discovery::KeepGoing(gaps.clone()),
        )
        .try_collect()
        .await?;
    assert!(addrs.is_empty());
    let report = gaps.report();
    assert_eq!(report.groups[0].stage, Stage::List);
    assert!(got(&gets).is_empty());
    Ok(())
}

/// C35: a listed No that lies drops a real dep from a BUILD `query()`; the
/// consumer's def hash moves with it, so the wrong build never shares a key
/// with the right one; and `heph validate` reports the lie.
#[tokio::test]
async fn executor_query_lying_no_drops_dep_and_validate_reports() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        c1_targets(),
        Faults {
            name: Some("liar"),
            listed_labels: vec![(parse_addr("//p:test")?, vec![])],
            ..Default::default()
        },
    )?;
    assert!(
        deps(&ws, trust(&ws, TRUST), "label(test)")
            .await?
            .is_empty()
    );
    assert_eq!(
        deps(&ws, trust(&ws, IGNORE), "label(test)").await?,
        vec!["//p:test"]
    );
    let found = mismatches(&ws).await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("provider `liar` lists //p:test with labels [], but it resolves to labels [go-test, test]"),
        "{}",
        found[0]
    );
    Ok(())
}

/// C35: the dropped dep changes the consumer's key.
#[tokio::test]
async fn lying_no_changes_consumer_key() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        c1_targets(),
        Faults {
            listed_labels: vec![(parse_addr("//p:test")?, vec![])],
            ..Default::default()
        },
    )?;
    let q = parse_addr(&query_addr("label(test)"))?;
    let lied = Arc::clone(&ws.engine)
        .get_def(trust(&ws, TRUST), &q)
        .await?;
    let truth = Arc::clone(&ws.engine)
        .get_def(trust(&ws, IGNORE), &q)
        .await?;
    assert_ne!(lied.target_def.hash, truth.target_def.hash);
    Ok(())
}

/// C36: a listed Yes that lies on a target that exists is not a dep — on the
/// query-target walk a listed Yes is only a candidate, and the resolved spec
/// decides it as for a shrug — and `heph validate` reports the lie.
#[tokio::test]
async fn executor_query_lying_yes_is_reported_by_validate() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        c1_targets(),
        Faults {
            name: Some("liar"),
            listed_labels: vec![(parse_addr("//p:lib")?, strings(&["test"]))],
            ..Default::default()
        },
    )?;
    assert_eq!(
        deps(&ws, trust(&ws, TRUST), "label(test)").await?,
        vec!["//p:test"]
    );
    assert_eq!(got(&gets), vec!["//p:lib", "//p:test"]);
    let found = mismatches(&ws).await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("lists //p:lib with labels [test]"),
        "{}",
        found[0]
    );
    Ok(())
}

/// Providers `s` (registered first, so it resolves `//p:x`) and `l`, which
/// lists `//p:x` but declines it in `get`; each lists it with its own labels.
fn skipped_resolver(s_labels: &[&str], l_labels: &[&str]) -> anyhow::Result<htestkit::Workspace> {
    let s = FaultProvider::new(
        vec![bash("//p:x", s_labels)],
        Faults {
            name: Some("s"),
            ..Default::default()
        },
    )?;
    let l = FaultProvider::new(
        vec![],
        Faults {
            name: Some("l"),
            vanished: vec![(parse_addr("//p:x")?, strings(l_labels))],
            ..Default::default()
        },
    )?;
    builder(vec![s, l])
}

/// Regression (R2, R3): a walk that skips the resolver (`exclude_provider=s`)
/// still resolves the addr through it. Another lister's Yes must not make the
/// addr a dep the resolved spec does not match: the dep set is the same under
/// either trust.
#[tokio::test]
async fn executor_query_listed_yes_with_a_skipped_resolver() -> anyhow::Result<()> {
    let ws = skipped_resolver(&[], &["ci"])?;
    let q = format!("{},exclude_provider=s", query_addr("label(ci)"));
    let trusted = deps_of(&ws, trust(&ws, TRUST), &q).await?;
    let ignored = deps_of(&ws, trust(&ws, IGNORE), &q).await?;
    assert_eq!(trusted, ignored);
    assert!(trusted.is_empty(), "{trusted:?}");
    Ok(())
}

/// Accepted exemption (user, 2026-10-06): `s` resolves `//p:x` with `ci`, and
/// the walk, which excludes `s`, sees only `l`'s listed No. The No is trusted
/// and drops the dep; `heph validate` reports `l` as a lister that is not the
/// resolver, which is what gates it.
#[tokio::test]
async fn executor_query_listed_no_with_a_skipped_resolver_is_reported_by_validate()
-> anyhow::Result<()> {
    let ws = skipped_resolver(&["ci"], &[])?;
    let q = format!("{},exclude_provider=s", query_addr("label(ci)"));
    let trusted = deps_of(&ws, trust(&ws, TRUST), &q).await?;
    assert!(
        trusted.is_empty(),
        "the dropped dep is accepted: {trusted:?}"
    );
    let found = mismatches(&ws).await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0]
            .contains("provider `l` lists //p:x with listed facts, but provider `s` resolves it"),
        "{}",
        found[0]
    );
    Ok(())
}

/// C38: under `Ignore` the query-target walk reads only the addr, and makes
/// the `get_spec`s it always made.
#[tokio::test]
async fn kill_switch_disables_executor_trust() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        c1_targets(),
        Faults {
            listed_labels: vec![(parse_addr("//p:test")?, vec![])],
            ..Default::default()
        },
    )?;
    assert_eq!(
        deps(&ws, trust(&ws, IGNORE), "label(test)").await?,
        vec!["//p:test"],
        "a lying No changes nothing when facts are ignored"
    );
    assert_eq!(got(&gets), vec!["//p:format", "//p:lib", "//p:test"]);
    Ok(())
}

/// C39 (R1): a target labelled `x` whose deps are `query("label(x)")` with no
/// provider exclusion. Its own entry is a listed Yes; `get_spec` sees the cycle
/// and the caller is excluded from its own deps, as before.
#[tokio::test]
async fn self_referencing_query_excludes_caller_under_trust() -> anyhow::Result<()> {
    let q = format!("{},exclude_provider=__none__", query_addr("label(x)"));
    let a = Target {
        deps: HashMap::from([("q".to_string(), vec![q])]),
        ..bash("//pkg:a", &["x"])
    };
    let (ws, _) = workspace(vec![a, bash("//pkg:b", &["x"])], Faults::default())?;
    let a = parse_addr("//pkg:a")?;
    let mut seen = Vec::new();
    for t in [TRUST, IGNORE] {
        // One request: the query's spec is memoized as `a`'s own resolution
        // computed it, with `a` on the chain.
        let rs = trust(&ws, t);
        let def = tokio::time::timeout(
            Duration::from_secs(10),
            Arc::clone(&ws.engine).get_def(Arc::clone(&rs), &a),
        )
        .await
        .expect("get_def hung: the cycle was not detected")?;
        let q_addr = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == PACKAGE)
            .expect("the query input")
            .r#ref
            .r#ref
            .clone();
        let spec = Arc::clone(&ws.engine).get_spec(rs, &q_addr).await?;
        seen.push(format!("{:?}", spec.config.get("deps")));
    }
    assert_eq!(seen[0], seen[1], "{seen:?}");
    assert!(
        !seen[0].contains("//pkg:a"),
        "the caller is its own dep: {seen:?}"
    );
    assert!(seen[0].contains("//pkg:b"), "{seen:?}");
    Ok(())
}

/// Invariant 1: `heph r 'driver(…)'` admits its selection from listed facts:
/// a provider whose every entry lists another driver is never asked to `get`.
#[tokio::test]
async fn install_selection_honors_listed_facts() -> anyhow::Result<()> {
    let go = FaultProvider::new(
        vec![
            Target {
                driver: "go_compile".to_string(),
                ..bash("//p:lib", &["go-build"])
            },
            Target {
                driver: "exec".to_string(),
                ..bash("//p:test", &["test"])
            },
        ],
        Faults {
            name: Some("go"),
            ..Default::default()
        },
    )?;
    let go_gets = go.gets();
    let creds = FaultProvider::new(
        vec![bash("//auth:token", &[])],
        Faults {
            name: Some("creds"),
            ..Default::default()
        },
    )?;
    let ws = builder(vec![go, creds])?;
    let (res, events) = run(&ws, &driver("bash"), &ResultOptions::default()).await;
    res?;
    assert_eq!(built(&events), vec!["//auth:token"]);
    assert!(
        got(&go_gets).iter().all(|a| !a.starts_with("//p:")),
        "go resolved {:?}",
        got(&go_gets)
    );
    Ok(())
}

/// Invariant 1: `Engine::states` never reads listed facts — lying facts give
/// the same states as honest ones, under either trust. `states_under` is
/// covered by the engine's `states_under_ignores_listed_facts`: its executor
/// cannot be built from outside the engine.
#[tokio::test]
async fn states_under_ignores_facts() -> anyhow::Result<()> {
    let faults = |lie: bool| Faults {
        name: Some("go"),
        states: vec![(
            "foo".to_string(),
            HashMap::from([("k".to_string(), Value::Bool(true))]),
        )],
        listed_facts: lie
            .then(|| FactsFn::new(|_, _| Some(ListedFacts::default().with_labels(["lie"])))),
        ..Default::default()
    };
    let mut seen = Vec::new();
    for (lie, t) in [(false, TRUST), (true, TRUST), (true, IGNORE)] {
        let (ws, _) = workspace(vec![bash("//foo/x:t", &[])], faults(lie))?;
        let states = Arc::clone(&ws.engine)
            .states(
                trust(&ws, t),
                &everything(),
                &StatesOptions {
                    inherited: true,
                    provider: None,
                },
            )
            .await?;
        seen.push(format!(
            "{:?}",
            states
                .iter()
                .map(|p| (
                    p.package.as_str().to_string(),
                    p.states
                        .iter()
                        .map(|s| s.package.as_str().to_string())
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>()
        ));
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], seen[2]);
    assert!(seen[0].contains("foo/x"), "{}", seen[0]);
    Ok(())
}

/// Invariant 2, per field: the run trusts a lie that says Yes or No, and
/// `heph validate` reports both.
#[tokio::test]
async fn listed_fact_lie_is_trusted_by_the_run_and_reported_by_validate() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        vec![
            bash("//a:label_yes", &["y"]),
            bash("//a:label_no", &["x"]),
            bash("//a:driver_yes", &[]),
            bash("//a:driver_no", &[]),
            codegen("//a:codegen_no"),
            bash("//a:codegen_yes", &[]),
        ],
        Faults {
            name: Some("liar"),
            listed_facts: facts_fn(|a| {
                let base = ListedFacts::default();
                Some(match a.name.as_str() {
                    "label_yes" => base.with_labels(["x"]).with_driver("bash"),
                    "label_no" => base.with_labels(Vec::<String>::new()).with_driver("bash"),
                    "driver_yes" => base.with_driver("go_compile"),
                    "driver_no" => base.with_driver("exec"),
                    "codegen_no" => base.with_has_codegen(false),
                    "codegen_yes" => base.with_has_codegen(true),
                    _ => return None,
                })
            }),
            ..Default::default()
        },
    )?;
    let rs = || trust(&ws, TRUST);
    assert_eq!(select(&ws, rs(), &label("x")).await?, vec!["//a:label_yes"]);
    assert_eq!(
        select(&ws, rs(), &driver("go_compile")).await?,
        vec!["//a:driver_yes"]
    );
    let bash_selected = select(&ws, rs(), &driver("bash")).await?;
    assert!(!bash_selected.contains(&"//a:driver_no".to_string()));
    assert!(
        !select(&ws, rs(), &Matcher::TreeOutputTo(PkgBuf::from("")))
            .await?
            .contains(&"//a:codegen_no".to_string()),
        "a listed has_codegen=false is trusted"
    );

    let found = mismatches(&ws).await?;
    for want in [
        "lists //a:label_yes with labels [x], but it resolves to labels [y]",
        "lists //a:label_no with labels [], but it resolves to labels [x]",
        "lists //a:driver_yes with driver go_compile, but it resolves to driver bash",
        "lists //a:driver_no with driver exec, but it resolves to driver bash",
        "lists //a:codegen_no with has_codegen false, but it resolves to has_codegen true",
        "lists //a:codegen_yes with has_codegen true, but it resolves to has_codegen false",
    ] {
        assert!(
            found.iter().any(|f| f.contains(want)),
            "{want} missing from {found:#?}"
        );
    }
    assert_eq!(found.len(), 6, "{found:#?}");
    Ok(())
}

/// Invariant 2: honest listings of every field, and a phantom with facts,
/// report nothing.
#[tokio::test]
async fn honest_listings_report_no_mismatch() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        vec![
            bash("//a:x", &["x"]),
            bash("//a:none", &[]),
            codegen("//a:gen"),
        ],
        Faults {
            vanished: vec![(parse_addr("//a:ghost")?, strings(&["x"]))],
            listed_facts: Some(FactsFn::new(|a, _| match a.name.as_str() {
                "gen" => Some(
                    ListedFacts::default()
                        .with_labels(Vec::<String>::new())
                        .with_driver("bash")
                        .with_has_codegen(true),
                ),
                "x" => Some(
                    ListedFacts::default()
                        .with_labels(["x"])
                        .with_driver("bash")
                        .with_has_codegen(false),
                ),
                _ => None,
            })),
            ..Default::default()
        },
    )?;
    assert_eq!(mismatches(&ws).await?, Vec::<String>::new());
    Ok(())
}

/// Invariant 2: a listed driver match whose `get` fails keeps the walk's rules
/// (a recorded skip under keep-going, a failed run without), and a listed
/// `has_codegen=false` never reaches the failing `get` at all.
#[tokio::test]
async fn listed_match_failures_follow_the_walk_rules() -> anyhow::Result<()> {
    let faults = || -> anyhow::Result<Faults> {
        Ok(Faults {
            fail_get: vec![parse_addr("//a:bad")?],
            ..Default::default()
        })
    };
    let targets = || vec![bash("//a:ok", &[]), bash("//a:bad", &[])];

    let (ws, _) = workspace(targets(), faults()?)?;
    let gaps = Gaps::new("driver(bash)");
    let opts = ResultOptions {
        discovery: Discovery::KeepGoing(gaps.clone()),
        ..Default::default()
    };
    let (res, events) = run(&ws, &driver("bash"), &opts).await;
    assert!(res?.errors.is_empty());
    assert_eq!(built(&events), vec!["//a:ok"]);
    let report = gaps.report();
    assert_eq!(report.skipped, 1, "{report:?}");
    assert_eq!(report.groups[0].stage, Stage::Spec);

    let (ws, _) = workspace(targets(), faults()?)?;
    let (res, _) = run(&ws, &driver("bash"), &ResultOptions::default()).await;
    assert!(
        res.is_err(),
        "without keep-going a failing match fails the run"
    );

    let (ws, gets) = workspace(
        targets(),
        Faults {
            listed_facts: facts_fn(|_| Some(ListedFacts::default().with_has_codegen(false))),
            ..faults()?
        },
    )?;
    assert!(
        select(
            &ws,
            trust(&ws, TRUST),
            &Matcher::TreeOutputTo(PkgBuf::from(""))
        )
        .await?
        .is_empty()
    );
    assert!(got(&gets).is_empty(), "{:?}", got(&gets));
    Ok(())
}

/// A cancellation while a package is being listed, on a walk that merges
/// listings, ends the walk with every package task joined: nothing still holds
/// the request.
#[tokio::test]
async fn cancel_during_listing_joins_tasks() -> anyhow::Result<()> {
    let started = Arc::new(tokio::sync::Notify::new());
    let mut targets = c1_targets();
    targets.extend((0..16).map(|i| bash(&format!("//q{i}:t"), &["test"])));
    let (ws, _) = workspace(
        targets,
        Faults {
            cancel_at: Some((Stage::List, "q3".to_string(), Arc::clone(&started))),
            ..Default::default()
        },
    )?;
    let rs = trust(&ws, TRUST);
    let walk = tokio::spawn({
        let (engine, rs) = (Arc::clone(&ws.engine), Arc::clone(&rs));
        async move {
            let m = label("test");
            engine
                .query(rs, &m, Discovery::Complete)
                .try_collect::<Vec<Addr>>()
                .await
        }
    });
    started.notified().await;
    rs.ctoken().cancel();
    let res = tokio::time::timeout(Duration::from_secs(10), walk).await??;
    assert!(res.is_err(), "a cancelled walk is not a complete one");
    assert_eq!(
        Arc::strong_count(&rs),
        1,
        "a package task still holds the request"
    );
    Ok(())
}

/// With every field unknown, the walk makes exactly the `get`s it made
/// before listed facts, on every matcher kind that reads one.
#[tokio::test]
async fn shrug_everywhere_adds_no_get() -> anyhow::Result<()> {
    for m in [
        label("test"),
        driver("bash"),
        Matcher::Not(Box::new(label("test"))),
        Matcher::And(vec![label("test"), driver("bash")]),
    ] {
        let (ws, gets) = workspace(
            c1_targets(),
            Faults {
                facts_unknown: true,
                ..Default::default()
            },
        )?;
        let trusted = select(&ws, trust(&ws, TRUST), &m).await?;
        let trusted_gets = gets.addrs().len();
        let (ws, gets) = workspace(
            c1_targets(),
            Faults {
                facts_unknown: true,
                ..Default::default()
            },
        )?;
        let ignored = select(&ws, trust(&ws, IGNORE), &m).await?;
        assert_eq!(trusted, ignored, "{m:?}");
        assert_eq!(trusted_gets, gets.addrs().len(), "{m:?}");
        assert_eq!(trusted_gets, 3, "{m:?}");
    }
    Ok(())
}

/// The list counter sees one `list` per package per walk — facts add none.
#[tokio::test]
async fn facts_add_no_list_call() -> anyhow::Result<()> {
    let mut counts = Vec::new();
    for t in [TRUST, IGNORE] {
        let provider = FaultProvider::new(c1_targets(), Faults::default())?;
        let lists = provider.list_calls();
        let ws = builder(vec![provider])?;
        select(&ws, trust(&ws, t), &label("test")).await?;
        counts.push(lists.load(Ordering::SeqCst));
    }
    assert_eq!(counts[0], counts[1], "{counts:?}");
    Ok(())
}
