//! Provider cache scopes, end to end: the real engine minting a scope per root
//! request and the real buildfile provider keeping what it read per scope.
//!
//! Every BUILD file here calls `heph.tally.tick()`, a test plugin function that
//! counts evaluations per package, so "evaluated once" and "evaluated again"
//! are read off the provider's actual work rather than inferred from answers.
//! A [`FaultProvider`] with no targets sits beside the buildfile provider: it
//! reports which scope each call carried and when the engine ended one.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use anyhow::Context as _;
use futures::TryStreamExt as _;
use hcore::htvalue::Value;
use hcore::htvalue::signature::{FnSignature, ParamType};
use heph::engine::fault_provider::{FaultProvider, Faults, ScopeLog};
use heph::engine::request_state::RequestState;
use heph::engine::{Discovery, OutputMatcher, PluginParts, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use hplugin::function::{FnArgs, FnCallContext, FnOutcome, PluginFn, PluginFnDef};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long any wait in this file may take before the test fails instead of
/// hanging.
const DEADLINE: Duration = Duration::from_secs(60);

/// Holds the first evaluation that calls `tick()` inside the call — on the
/// `hcore::blocking` thread evaluating the package — until released.
struct Hold {
    entered: tokio::sync::Notify,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

/// `heph.tally.tick()`: counts one evaluation of the calling package.
#[derive(Default)]
struct Tally {
    evals: Mutex<BTreeMap<String, usize>>,
    hold: Option<Arc<Hold>>,
}

impl Tally {
    fn evals(&self) -> BTreeMap<String, usize> {
        self.evals.lock().expect("tally lock").clone()
    }

    fn evals_of(&self, pkg: &str) -> usize {
        self.evals().get(pkg).copied().unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl PluginFn for Tally {
    async fn call(&self, ctx: &FnCallContext<'_>, _args: FnArgs) -> anyhow::Result<FnOutcome> {
        *self
            .evals
            .lock()
            .expect("tally lock")
            .entry(ctx.pkg.to_string())
            .or_default() += 1;
        if let Some(hold) = &self.hold {
            // Only the first call takes the receiver; every later one passes.
            let rx = hold.release.lock().expect("hold lock").take();
            if let Some(rx) = rx {
                hold.entered.notify_one();
                // A blocking wait on purpose: this is the straggler, a package
                // evaluation parked on a blocking thread past its scope's end.
                rx.recv_timeout(DEADLINE)
                    .map_err(|e| anyhow::anyhow!("hold never released: {e}"))?;
            }
        }
        Ok(FnOutcome::from(Value::String(String::new())))
    }
}

struct Ws {
    ws: htestkit::Workspace,
    tally: Arc<Tally>,
    scopes: Arc<ScopeLog>,
}

impl Ws {
    fn new(tally: Tally) -> anyhow::Result<Self> {
        let tally = Arc::new(tally);
        let fault = FaultProvider::new(vec![], Faults::default())?;
        let scopes = fault.scopes();
        let func: Arc<dyn PluginFn> = Arc::clone(&tally) as _;
        let ws = htestkit::WorkspaceBuilder::new()?
            .with_provider(move |_| Box::new(fault))
            .with_provider(|init| {
                Box::new(heph::pluginbuildfile::Provider::new(
                    init.root.to_path_buf(),
                    init.runtime.clone(),
                    Arc::clone(&init.functions),
                ))
            })
            .with_plugin("tally", move |_| {
                Ok(PluginParts::default().with_functions(vec![PluginFnDef {
                    name: "tick".to_string(),
                    signature: FnSignature {
                        positional: vec![],
                        named: vec![],
                        variadic: None,
                        returns: ParamType::String,
                    },
                    doc: "Count one evaluation of the calling package.".to_string(),
                    func,
                }]))
            })
            .with_managed_driver(Box::new(
                heph::pluginexec::Driver::new_bash().with_host_path(),
            ))
            .build()?;
        Ok(Self { ws, tally, scopes })
    }

    /// A package whose BUILD file counts its evaluation and defines `names`,
    /// each a trivial bash target.
    fn write_pkg(&self, pkg: &str, names: &[&str]) {
        let mut build = String::from("_t = heph.tally.tick()\n");
        for name in names {
            build.push_str(&format!(
                "target(name = \"{name}\", driver = \"bash\", run = \"echo {name} > $OUT\", out = \"o.txt\")\n"
            ));
        }
        self.ws.write_build_file(pkg, &build);
    }

    /// Every target the workspace defines, as `rs` sees it.
    async fn all(&self, rs: &Arc<RequestState>) -> anyhow::Result<Vec<String>> {
        let m = Matcher::PackagePrefix(PkgBuf::from(""));
        let addrs: Vec<Addr> = Arc::clone(&self.ws.engine)
            .query(Arc::clone(rs), &m, Discovery::Complete)
            .try_collect()
            .await?;
        let mut v: Vec<String> = addrs.iter().map(Addr::format).collect();
        v.sort();
        Ok(v)
    }

    /// Whether `addr` resolves to a spec in `rs`.
    async fn exists(&self, rs: &Arc<RequestState>, addr: &str) -> anyhow::Result<bool> {
        match Arc::clone(&self.ws.engine)
            .get_spec(Arc::clone(rs), &parse_addr(addr)?)
            .await
        {
            Ok(_) => Ok(true),
            Err(e)
                if e.downcast_ref::<heph::engine::error::TargetNotFoundError>()
                    .is_some() =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// The distinct scopes the providers were called in, in first-seen order.
    fn seen_scopes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for s in self.scopes.seen().into_iter().flatten() {
            if !out.contains(&s.get()) {
                out.push(s.get());
            }
        }
        out
    }
}

/// Wait for `cond`, failing after [`DEADLINE`].
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Drop `rs` and wait until the engine has told every provider its scope
/// ended: the hook is counted on the request's background counter.
async fn end_root(ws: &Ws, rs: Arc<RequestState>) {
    let bg = rs.bg_pending();
    let ended_before = ws.scopes.ended().len();
    drop(rs);
    wait_for("the scope to end", || {
        ws.scopes.ended().len() > ended_before
    })
    .await;
    wait_for("the scope hook to drain", || {
        bg.load(Ordering::Acquire) == 0
    })
    .await;
}

/// `codegen = "in_place"` resolves each in-place target again under a fresh
/// hash-only request (the write-back guard, the fixpoint hashin). Those
/// requests inherit the run's scope, so the run evaluates each BUILD file once
/// however many in-place targets it has.
#[tokio::test]
async fn in_place_targets_reuse_the_runs_evaluations() -> anyhow::Result<()> {
    let ws = Ws::new(Tally::default())?;
    for pkg in ["fix", "fix2"] {
        let mut build = String::from("_t = heph.tally.tick()\n");
        for n in 1..=3 {
            build.push_str(&format!(
                r#"
target(
    name = "up{n}",
    driver = "bash",
    deps = [file("src{n}.txt")],
    out = "src{n}.txt",
    codegen = "in_place",
    run = "up=$(tr a-z A-Z < src{n}.txt); printf '%s' \"$up\" > $OUT",
)
"#
            ));
            ws.ws
                .write_file(&format!("{pkg}/src{n}.txt"), &format!("hello {n}"));
        }
        ws.ws.write_build_file(pkg, &build);
    }

    let m = Matcher::Or(vec![
        Matcher::Package(PkgBuf::from("fix")),
        Matcher::Package(PkgBuf::from("fix2")),
    ]);
    let results = ws.ws.run_matcher(&m).await?;
    assert_eq!(results.len(), 6, "every in-place target ran");
    for pkg in ["fix", "fix2"] {
        for n in 1..=3 {
            assert_eq!(
                std::fs::read_to_string(ws.ws.dir.path().join(format!("{pkg}/src{n}.txt")))?,
                format!("HELLO {n}"),
                "{pkg}/src{n}.txt was written back, so the in-place path ran",
            );
        }
    }
    assert_eq!(
        ws.tally.evals(),
        BTreeMap::from([("fix".to_string(), 1), ("fix2".to_string(), 1)]),
        "one evaluation per BUILD file for the whole run",
    );
    assert_eq!(
        ws.seen_scopes().len(),
        1,
        "every provider call of the run carried one scope"
    );
    Ok(())
}

/// One engine, two root requests in turn: the second sees the BUILD file as
/// it is after an edit — a target added, a package added — not the first
/// request's evaluation of it.
#[tokio::test]
async fn second_request_sees_an_edited_build_file() -> anyhow::Result<()> {
    let ws = Ws::new(Tally::default())?;
    ws.write_pkg("p", &["a"]);

    let rs1 = ws.ws.engine.new_state();
    assert_eq!(ws.all(&rs1).await?, vec!["//p:a"]);
    assert!(!ws.exists(&rs1, "//p:b").await?);
    end_root(&ws, rs1).await;

    ws.write_pkg("p", &["a", "b"]);
    ws.write_pkg("q", &["x"]);

    let rs2 = ws.ws.engine.new_state();
    assert_eq!(
        ws.all(&rs2).await?,
        vec!["//p:a", "//p:b", "//q:x"],
        "the next root request lists the added target and package",
    );
    assert!(
        ws.exists(&rs2, "//p:b").await?,
        "and resolves the added target"
    );
    assert_eq!(ws.tally.evals_of("p"), 2, "p evaluated once per request");
    let scopes = ws.seen_scopes();
    assert_eq!(scopes.len(), 2, "two root requests, two scopes: {scopes:?}");
    Ok(())
}

/// Two root requests alive at once in one engine, the BUILD file edited
/// between their first reads and again after: each answers from its own
/// snapshot, taken by its own first read.
#[tokio::test]
async fn concurrent_root_requests_do_not_share_snapshots() -> anyhow::Result<()> {
    let ws = Ws::new(Tally::default())?;
    ws.write_pkg("p", &["a"]);

    let rs1 = ws.ws.engine.new_state();
    assert_eq!(ws.all(&rs1).await?, vec!["//p:a"]);

    ws.write_pkg("p", &["a", "b"]);
    let rs2 = ws.ws.engine.new_state();
    assert_eq!(
        ws.all(&rs2).await?,
        vec!["//p:a", "//p:b"],
        "a request started after the edit reads the edited file, though another request read it \
         before",
    );

    ws.write_pkg("p", &["a", "b", "c"]);
    // Names neither request has asked the engine for, so the answers come
    // from the provider's snapshot of each scope, not from the engine's
    // per-request memo.
    assert!(
        !ws.exists(&rs1, "//p:b").await?,
        "the first request keeps its snapshot: no `b`"
    );
    assert!(!ws.exists(&rs1, "//p:c").await?, "nor `c`");
    assert!(
        !ws.exists(&rs2, "//p:c").await?,
        "the second request keeps its own snapshot: no `c`"
    );
    assert_eq!(ws.tally.evals_of("p"), 2, "one evaluation per scope");

    let rs3 = ws.ws.engine.new_state();
    assert!(ws.exists(&rs3, "//p:c").await?, "a third request sees `c`");
    drop((rs1, rs2, rs3));
    Ok(())
}

/// A package evaluation still running on its `hcore::blocking` thread when its
/// scope ends: what it writes when it finishes reaches no later scope. The
/// next request re-evaluates the edited BUILD file, while the straggler is
/// still parked and after it finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scope_end_while_pkg_eval_in_flight() -> anyhow::Result<()> {
    let (release, rx) = std::sync::mpsc::channel();
    let hold = Arc::new(Hold {
        entered: tokio::sync::Notify::new(),
        release: Mutex::new(Some(rx)),
    });
    let ws = Ws::new(Tally {
        hold: Some(Arc::clone(&hold)),
        ..Default::default()
    })?;
    ws.write_pkg("p", &["a"]);

    // Root request 1 starts evaluating `p` and parks inside it.
    let rs1 = ws.ws.engine.new_state();
    let bg1 = rs1.bg_pending();
    let engine = Arc::clone(&ws.ws.engine);
    let task = tokio::spawn(async move {
        let addr = parse_addr("//p:a").expect("addr");
        engine.get_spec(rs1, &addr).await.map(|_| ())
    });
    tokio::time::timeout(DEADLINE, hold.entered.notified())
        .await
        .context("the evaluation of p never started")?;

    // Abandon it: the request state drops and the scope ends while the
    // evaluation is still parked on its blocking thread.
    task.abort();
    assert!(task.await.is_err_and(|e| e.is_cancelled()));
    wait_for("scope 1 to end", || ws.scopes.ended().len() == 1).await;
    wait_for("scope 1's hook to drain", || {
        bg1.load(Ordering::Acquire) == 0
    })
    .await;
    assert_eq!(
        ws.tally.evals_of("p"),
        1,
        "the straggler is still in flight"
    );

    // The tree moves on; the next request reads it while the straggler is
    // still parked...
    ws.write_pkg("p", &["a", "b"]);
    let rs2 = ws.ws.engine.new_state();
    let engine = Arc::clone(&ws.ws.engine);
    let second = tokio::spawn(async move {
        let found = Arc::clone(&engine)
            .get_spec(Arc::clone(&rs2), &parse_addr("//p:b")?)
            .await;
        drop(rs2);
        found.map(|_| ())
    });
    // ...and the straggler finishes, writing the old `p` into its detached
    // bucket.
    release.send(()).context("the straggler stopped waiting")?;
    tokio::time::timeout(DEADLINE, second)
        .await
        .context("request 2 never finished")??
        .map_err(|e| e.context("request 2 must see `b`, not the straggler's evaluation"))?;

    // With the straggler released, a third request still evaluates afresh.
    ws.write_pkg("p", &["a", "b", "c"]);
    let rs3 = ws.ws.engine.new_state();
    assert_eq!(
        ws.all(&rs3).await?,
        vec!["//p:a", "//p:b", "//p:c"],
        "the late write recreated no bucket that a later request reads",
    );
    assert_eq!(ws.tally.evals_of("p"), 3, "straggler, request 2, request 3");
    Ok(())
}

/// The scope is the provider's business: the same target in two root
/// requests of one engine, the tree unchanged, has the same def hash and the
/// same hashin, and runs with the same environment and argv.
#[tokio::test]
async fn scope_id_never_reaches_a_target() -> anyhow::Result<()> {
    let ws = Ws::new(Tally::default())?;
    ws.ws.write_build_file(
        "p",
        r#"
_t = heph.tally.tick()
target(
    name = "env",
    driver = "bash",
    run = "env | sort > $OUT; echo \"argv: $0 $*\" >> $OUT",
    out = "env.txt",
    cache = False,
)
"#,
    );
    let addr = parse_addr("//p:env")?;

    let mut seen = Vec::new();
    for _ in 0..2 {
        let rs = ws.ws.engine.new_state();
        let engine = Arc::clone(&ws.ws.engine);
        let def = Arc::clone(&engine).get_def(Arc::clone(&rs), &addr).await?;
        let hashin = Arc::clone(&engine)
            .meta(Arc::clone(&rs), &addr)
            .await?
            .hashin
            .clone();
        let result = Arc::clone(&engine)
            .result_addr(
                Arc::clone(&rs),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        let out = crate::common::artifact_string(&result);
        seen.push((def.target_def.hash.clone(), hashin, out));
        end_root(&ws, rs).await;
    }

    let scopes = ws.seen_scopes();
    assert_eq!(scopes.len(), 2, "two roots, two scopes: {scopes:?}");
    assert_eq!(ws.tally.evals_of("p"), 2, "each root evaluated p itself");
    let (first, second) = (&seen[0], &seen[1]);
    assert_eq!(first.0, second.0, "same def hash in both scopes");
    assert_eq!(first.1, second.1, "same hashin in both scopes");
    assert!(
        first.2.contains("argv: "),
        "the target ran and wrote its env: {}",
        first.2
    );
    assert_eq!(
        first.2, second.2,
        "the target's env and argv are the same in both scopes"
    );
    Ok(())
}
