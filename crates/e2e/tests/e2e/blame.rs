//! Who a failure is blamed on (error-reporting D1): the culprit a request
//! records — the target whose own spec, def or run failed, the declarer of a
//! dependency that does not exist, or a BUILD file that did not evaluate —
//! never the dependent that happened to resolve it.
//!
//! Each test reads the request's failure registry (`take_failures`), which
//! holds one record per culprit. What the report prints about it (`path`,
//! `more`, `reached by:`, JSON) is the next stage's; these pin the blame.
//! The pure classification rules are unit tests next to `failure_fate` in
//! `crates/engine/src/engine/result.rs`.
#![expect(
    clippy::panic,
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use hcore::hmemoizer::downcast_chain_ref;
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::fault_provider::{FaultProvider, Faults};
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use hplugin::error::{
    CancelledError, Culprit, CycleError, MissingDep, Stage, StageFrame, TargetFailure,
};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// What one request left behind.
struct Run {
    err: Option<anyhow::Error>,
    failures: Vec<Arc<TargetFailure>>,
}

impl Run {
    /// Every recorded culprit, sorted.
    fn culprits(&self) -> Vec<String> {
        let mut c: Vec<String> = self
            .failures
            .iter()
            .map(|f| f.culprit().to_string())
            .collect();
        c.sort();
        c
    }

    /// The one recorded failure, or a panic listing them.
    fn only(&self) -> &TargetFailure {
        match self.failures.as_slice() {
            [f] => f,
            fs => panic!(
                "one recorded failure, got {}:\n{}",
                fs.len(),
                self.describe()
            ),
        }
    }

    fn describe(&self) -> String {
        self.failures
            .iter()
            .map(|f| format!("{} (addr {}): {:#}", f.culprit(), f.addr.format(), f.source))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The one culprit, which must be the target `want`.
    fn assert_blames(&self, want: &str) -> &TargetFailure {
        let f = self.only();
        assert_eq!(
            f.culprit(),
            Culprit::Target(addr(want)),
            "blamed:\n{}",
            self.describe()
        );
        assert_eq!(f.addr.format(), want);
        f
    }
}

fn addr(a: &str) -> Addr {
    parse_addr(a).unwrap_or_else(|e| panic!("parse {a}: {e:#}"))
}

/// `matcher` in a fresh request with fail-fast off.
async fn run_matcher(engine: &Arc<Engine>, m: &Matcher) -> Run {
    let rs = engine.new_state_with_fail_fast(false);
    let res = Arc::clone(engine)
        .result(rs.clone(), m, OutputMatcher::All, &ResultOptions::default())
        .await;
    let failures = rs.take_failures();
    drop(rs);
    // A selection's per-target errors ride the batch; the request error is
    // only for one that failed as a whole.
    let err = match res {
        Ok(batch) => batch.errors.into_iter().next().map(|(_, e)| e),
        Err(e) => Some(e),
    };
    Run { err, failures }
}

/// `heph run <a>`, which must fail.
async fn run(engine: &Arc<Engine>, a: &str) -> Run {
    let r = run_matcher(engine, &Matcher::Addr(addr(a))).await;
    assert!(r.err.is_some(), "{a} must fail");
    r
}

/// The [`MissingDep`] on a recorded failure, or a panic.
fn missing_dep(f: &TargetFailure) -> &MissingDep {
    downcast_chain_ref::<MissingDep>(&f.source)
        .unwrap_or_else(|| panic!("a MissingDep on {:#}", f.source))
}

/// A BUILD-file workspace: `files` is `(package, content)`.
fn build_ws(files: &[(&str, &str)]) -> Workspace {
    let ws = Workspace::new();
    for (pkg, content) in files {
        ws.write_build_file(pkg, content);
    }
    ws
}

/// A bash target writing `$OUT`, in BUILD-file syntax, with `extra` options.
fn t(name: &str, extra: &str) -> String {
    format!(
        r#"target(name = "{name}", driver = "bash", run = "echo ok > $OUT", out = "o.txt", {extra})"#
    )
}

/// `//a:root → //b:x → //c:y`, `//c:y` carrying `y_extra`: the chain every
/// missing-ref test hangs its fault off, two hops below the request.
fn chain_to_c(y_extra: &str, more: &[(&str, &str)]) -> Workspace {
    let a = t("root", r#"deps = ["//b:x"]"#);
    let b = t("x", r#"deps = ["//c:y"]"#);
    let c = t("y", y_extra);
    let mut files = vec![("a", a.as_str()), ("b", b.as_str()), ("c", c.as_str())];
    files.extend_from_slice(more);
    build_ws(&files)
}

/// `d/BUILD` evaluates and defines only `present`.
const D_PRESENT: &str =
    r#"target(name = "present", driver = "bash", run = "echo ok > $OUT", out = "o.txt")"#;

/// The bash target the static provider serves.
fn bash(a: &str, run: &str, deps: &[&str]) -> Target {
    Target {
        addr: a.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        deps: HashMap::from([(
            String::new(),
            deps.iter().map(|d| (*d).to_string()).collect(),
        )]),
        ..Default::default()
    }
}

/// What [`FaultyBash`] fails, by address.
#[derive(Default, Clone)]
struct DriverFaults {
    parse: Vec<Addr>,
    apply_transitive: Vec<Addr>,
}

/// The managed bash driver, failing `parse` / `apply_transitive` of the
/// targets [`DriverFaults`] names: the driver stages a provider fault
/// cannot reach.
struct FaultyBash {
    inner: pluginexec::Driver,
    faults: DriverFaults,
}

#[async_trait::async_trait]
impl heph::engine::driver_managed::ManagedDriver for FaultyBash {
    fn config(
        &self,
        req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        heph::engine::driver_managed::ManagedDriver::config(&self.inner, req)
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        heph::engine::driver_managed::ManagedDriver::schema(&self.inner)
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        if self.faults.parse.contains(&req.target_spec.addr) {
            anyhow::bail!("injected error at parse {}", req.target_spec.addr.format());
        }
        heph::engine::driver_managed::ManagedDriver::parse(&self.inner, req, ctoken).await
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        if self.faults.apply_transitive.contains(&req.target_def.addr) {
            anyhow::bail!(
                "injected error at apply_transitive {}",
                req.target_def.addr.format()
            );
        }
        heph::engine::driver_managed::ManagedDriver::apply_transitive(&self.inner, req, ctoken)
            .await
    }
    async fn run<'a, 'io>(
        &self,
        req: heph::engine::driver_managed::ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<heph::engine::driver_managed::ManagedRunResponse> {
        heph::engine::driver_managed::ManagedDriver::run(&self.inner, req, ctoken).await
    }
}

/// STAGE's graph: `//f:root → [//f:l1, //f:s1, //f:s2, //f:s3]`,
/// `//f:l1 → //f:l2 → //f:l3 → //g:leaf`, every target bash. `//g:leaf`
/// contributes a transitive env, so every `l*` def applies transitives.
/// `targets` edits the set before it is served (an unknown driver, a failing
/// run).
fn stage_ws(
    faults: Faults,
    driver: DriverFaults,
    edit: impl FnOnce(&mut Vec<Target>),
) -> anyhow::Result<htestkit::Workspace> {
    let ok = "echo ok > $OUT";
    let mut leaf = bash("//g:leaf", ok, &[]);
    leaf.transitive.env.insert(
        "LEAF".to_string(),
        hplugin::driver::sandbox::Env {
            value: hplugin::driver::sandbox::EnvValue::Literal("1".to_string()),
            hash: true,
            append: false,
            append_prefix: String::new(),
        },
    );
    // Every target transitively over `//g:leaf` gets its env, so each `l*`
    // and the root apply transitives.
    let mut targets = vec![
        bash("//f:root", ok, &["//f:l1", "//f:s1", "//f:s2", "//f:s3"]),
        bash("//f:l1", ok, &["//f:l2"]),
        bash("//f:l2", ok, &["//f:l3"]),
        bash("//f:l3", ok, &["//g:leaf"]),
        bash("//f:s1", ok, &[]),
        bash("//f:s2", ok, &[]),
        bash("//f:s3", ok, &[]),
        leaf,
    ];
    edit(&mut targets);
    let provider = FaultProvider::new(targets, faults)?;
    htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(FaultyBash {
            inner: pluginexec::Driver::new_bash(),
            faults: driver,
        }))
        .build()
}

fn set_run(targets: &mut [Target], a: &str, run: &str) {
    let t = targets
        .iter_mut()
        .find(|t| t.addr == a)
        .unwrap_or_else(|| panic!("{a} in the set"));
    t.run = Some(run.to_string());
}

/// Nothing on the way down is blamed: no record for any address in `not`.
fn assert_not_blamed(r: &Run, not: &[&str]) {
    for a in not {
        assert!(
            !r.failures.iter().any(|f| f.addr.format() == *a),
            "{a} must not be blamed:\n{}",
            r.describe()
        );
    }
}

// ---------------------------------------------------------------------------
// A BUILD file that does not evaluate (U2)
// ---------------------------------------------------------------------------

/// EVAL-16: `//a:t → //b:t → //c:t → //d:t`, `d/BUILD` a parse error. The
/// file is the culprit, entered through `//d:t`; no target is.
#[tokio::test]
async fn deep_broken_build_blames_the_file() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("t", r#"deps = ["//b:t"]"#)),
        ("b", &t("t", r#"deps = ["//c:t"]"#)),
        ("c", &t("t", r#"deps = ["//d:t"]"#)),
        ("d", "target(name = \"t\"\nbroken(\n"),
    ]);
    let r = run(&ws.engine, "//a:t").await;
    let f = r.only();
    assert_eq!(
        f.culprit(),
        Culprit::File(Arc::from("d/BUILD")),
        "{}",
        r.describe()
    );
    assert_eq!(f.file.as_deref(), Some("d/BUILD"));
    assert_eq!(
        f.addr.format(),
        "//d:t",
        "the address that led into the file"
    );
    // stage B: `path` ending at //d:t, `addr: null`, `reached by:`.
    Ok(())
}

/// U2 / D1.d: two requested targets of one broken `d/BUILD` are one
/// failure, the file's.
#[tokio::test]
#[ignore = "stage A gap: a selection's probe of the broken package fails in discovery \
            (batch.errors), before result_addr, so nothing is recorded against d/BUILD"]
async fn broken_build_one_box_for_two_requested_targets() -> anyhow::Result<()> {
    let ws = build_ws(&[(
        "d",
        &format!("{}\n{}\nfail(\"boom\")\n", t("t1", ""), t("t2", "")),
    )]);
    let r = run_matcher(
        &ws.engine,
        &Matcher::Or(vec![
            Matcher::Addr(addr("//d:t1")),
            Matcher::Addr(addr("//d:t2")),
        ]),
    )
    .await;
    let err = r.err.as_ref().expect("the selection must fail");
    assert_eq!(r.culprits(), ["d/BUILD"], "{}\nerr: {err:#}", r.describe());
    Ok(())
}

/// D1.c rule 5: `a → b → c → d` with `d/BUILD` failing at evaluation (not
/// parse): the file, never `//c:t`, which only resolved it.
#[tokio::test]
async fn four_deep_broken_build_blames_file_not_c() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("t", r#"deps = ["//b:t"]"#)),
        ("b", &t("t", r#"deps = ["//c:t"]"#)),
        ("c", &t("t", r#"deps = ["//d:t"]"#)),
        ("d", &format!("{}\nfail(\"d is broken\")\n", t("t", ""))),
    ]);
    let r = run(&ws.engine, "//a:t").await;
    assert_eq!(r.culprits(), ["d/BUILD"], "{}", r.describe());
    assert!(
        !r.failures
            .iter()
            .any(|f| f.culprit() == Culprit::Target(addr("//c:t"))),
        "{}",
        r.describe()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// A declared ref that does not exist (D1.b)
// ---------------------------------------------------------------------------

/// GRAPH-04: `//c:y` declares `//d:missing` in `deps`; `d/BUILD` evaluates.
/// The declarer is the culprit, two hops below the request.
#[tokio::test]
async fn deep_missing_dep_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c(r#"deps = ["//d:missing"]"#, &[("d", D_PRESENT)]);
    let r = run(&ws.engine, "//a:root").await;
    let f = r.assert_blames("//c:y");
    let m = missing_dep(f);
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.declarer.format(), "//c:y");
    assert_eq!(m.origin_id, "deps");
    assert_not_blamed(&r, &["//a:root", "//b:x"]);
    // stage B: path //a:root → //b:x → //c:y.
    Ok(())
}

/// PR 1's interim text: before targets carry a source location, the
/// `MissingDep` names the declarer's BUILD file from its package.
#[tokio::test]
async fn missing_dep_interim_text_names_build_file() -> anyhow::Result<()> {
    let ws = chain_to_c(r#"deps = ["//d:missing"]"#, &[("d", D_PRESENT)]);
    let r = run(&ws.engine, "//a:root").await;
    let text = format!("{:#}", r.assert_blames("//c:y").source);
    assert!(
        text.contains("//c:y depends on //d:missing, which does not exist"),
        "{text}"
    );
    assert!(text.contains("the BUILD file in c/"), "{text}");
    Ok(())
}

#[tokio::test]
async fn missing_tool_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c(r#"tools = ["//d:missing"]"#, &[("d", D_PRESENT)]);
    let r = run(&ws.engine, "//a:root").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.origin_id, "tools");
    Ok(())
}

#[tokio::test]
async fn missing_runner_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c(r#"runner = "//d:missing""#, &[("d", D_PRESENT)]);
    let r = run(&ws.engine, "//a:root").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.origin_id, "runner");
    Ok(())
}

/// A group's member is the group's declaration: the group is blamed, not
/// the target that depends on it.
#[tokio::test]
async fn missing_group_member_blames_group() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("x", r#"deps = ["//g:grp"]"#)),
        (
            "g",
            r#"target(name = "grp", driver = "group", deps = ["//d:missing"])"#,
        ),
        ("d", D_PRESENT),
    ]);
    let r = run(&ws.engine, "//a:x").await;
    let m = missing_dep(r.assert_blames("//g:grp"));
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.declarer.format(), "//g:grp");
    Ok(())
}

/// A dep in a target's `transitive` is that target's declaration: the
/// consumer it is folded into did not name it.
#[tokio::test]
#[ignore = "stage A gap: input_defs blames the consumer //b:x; the merged input's origin \
            `_transitive_<hash>_<i>_dep_…` does not carry the declaring //c:y"]
async fn missing_transitive_dep_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c(
        r#"transitive = {"deps": ["//d:missing"]}"#,
        &[("d", D_PRESENT)],
    );
    let r = run(&ws.engine, "//a:root").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    Ok(())
}

#[tokio::test]
async fn missing_scratch_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c(r#"scratch = ["//d:missing"]"#, &[("d", D_PRESENT)]);
    let r = run(&ws.engine, "//a:root").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.origin_id, "scratch");
    Ok(())
}

#[tokio::test]
async fn missing_credential_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c(r#"credentials = ["//d:missing"]"#, &[("d", D_PRESENT)]);
    let r = run(&ws.engine, "//a:root").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.origin_id, "credentials");
    Ok(())
}

/// OPT-16: `${read://d:missing}` in `//c:y`'s `run`.
#[tokio::test]
async fn missing_read_producer_blames_consumer() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("root", r#"deps = ["//c:y"]"#)),
        (
            "c",
            r#"target(name = "y", driver = "bash", run = "echo ${read://d:missing} > $OUT", out = "o.txt")"#,
        ),
        ("d", D_PRESENT),
    ]);
    let r = run(&ws.engine, "//a:root").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.origin_id, "${read://}");
    Ok(())
}

// ---------------------------------------------------------------------------
// A ref of the wrong kind is the declarer's error (D1.c)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wrong_kind_scratch_ref_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c("", &[]);
    ws.write_build_file(
        "c",
        &format!(
            "{}\n{}\n",
            t("y", r#"scratch = ["//c:plain"]"#),
            t("plain", "")
        ),
    );
    let r = run(&ws.engine, "//a:root").await;
    r.assert_blames("//c:y");
    Ok(())
}

#[tokio::test]
async fn wrong_kind_credential_ref_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c("", &[]);
    ws.write_build_file(
        "c",
        &format!(
            "{}\n{}\n",
            t("y", r#"credentials = ["//c:plain"]"#),
            t("plain", "")
        ),
    );
    let r = run(&ws.engine, "//a:root").await;
    r.assert_blames("//c:y");
    Ok(())
}

// ---------------------------------------------------------------------------
// A stage fault two or more hops down blames the target it is about
// ---------------------------------------------------------------------------

/// OPT-19: `//app:bin → //lib:a → //lib:b`, `//lib:b` carrying an option
/// the driver refuses.
#[tokio::test]
async fn deep_bad_option_blames_the_target() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("app", &t("bin", r#"deps = ["//lib:a"]"#)),
        (
            "lib",
            &format!(
                "{}\n{}\n",
                t("a", r#"deps = [":b"]"#),
                t("b", r#"enviroment = {"X": "1"}"#)
            ),
        ),
    ]);
    let r = run(&ws.engine, "//app:bin").await;
    let f = r.assert_blames("//lib:b");
    assert!(
        format!("{:#}", f.source).contains("enviroment"),
        "{:#}",
        f.source
    );
    Ok(())
}

/// STAGE-02: the provider's `get` of `//f:l3` fails.
#[tokio::test]
async fn deep_get_error_blames_the_input() -> anyhow::Result<()> {
    let ws = stage_ws(
        Faults {
            fail_get: vec![addr("//f:l3")],
            ..Default::default()
        },
        DriverFaults::default(),
        |_| {},
    )?;
    let r = run(&ws.engine, "//f:root").await;
    r.assert_blames("//f:l3");
    assert_not_blamed(&r, &["//f:root", "//f:l1", "//f:l2"]);
    Ok(())
}

/// STAGE-03: the probe of `//g:leaf`'s package fails; the target resolved
/// in it is blamed, not the three above.
#[tokio::test]
async fn deep_probe_error_blames_the_package() -> anyhow::Result<()> {
    let ws = stage_ws(
        Faults {
            fail_probe: vec!["g".to_string()],
            ..Default::default()
        },
        DriverFaults::default(),
        |_| {},
    )?;
    let r = run(&ws.engine, "//f:root").await;
    let f = r.only();
    assert_eq!(f.addr.package.as_str(), "g", "{}", r.describe());
    assert_not_blamed(&r, &["//f:root", "//f:l1", "//f:l2", "//f:l3"]);
    Ok(())
}

/// STAGE-06: the driver's `parse` of `//f:l3` fails.
#[tokio::test]
async fn deep_parse_error_blames_the_input() -> anyhow::Result<()> {
    let ws = stage_ws(
        Faults::default(),
        DriverFaults {
            parse: vec![addr("//f:l3")],
            ..Default::default()
        },
        |_| {},
    )?;
    let r = run(&ws.engine, "//f:root").await;
    let f = r.assert_blames("//f:l3");
    assert!(
        format!("{:#}", f.source).contains("injected error at parse //f:l3"),
        "{:#}",
        f.source
    );
    Ok(())
}

/// STAGE-07: the driver's `apply_transitive` of `//f:l3` fails.
#[tokio::test]
async fn deep_apply_transitive_error_blames_the_input() -> anyhow::Result<()> {
    let ws = stage_ws(
        Faults::default(),
        DriverFaults {
            apply_transitive: vec![addr("//f:l3")],
            ..Default::default()
        },
        |_| {},
    )?;
    let r = run(&ws.engine, "//f:root").await;
    let f = r.assert_blames("//f:l3");
    assert!(
        format!("{:#}", f.source).contains("injected error at apply_transitive //f:l3"),
        "{:#}",
        f.source
    );
    Ok(())
}

/// STAGE-08: `//f:l3`'s own run fails. Right before stage A too; a
/// regression guard.
#[tokio::test]
async fn deep_run_error_blames_the_input() -> anyhow::Result<()> {
    let ws = stage_ws(Faults::default(), DriverFaults::default(), |ts| {
        set_run(ts, "//f:l3", "exit 1");
    })?;
    let r = run(&ws.engine, "//f:root").await;
    r.assert_blames("//f:l3");
    Ok(())
}

/// STAGE-10: `//f:l3`'s spec names a driver that does not exist.
#[tokio::test]
async fn deep_unknown_driver_blames_the_input() -> anyhow::Result<()> {
    let ws = stage_ws(Faults::default(), DriverFaults::default(), |ts| {
        if let Some(t) = ts.iter_mut().find(|t| t.addr == "//f:l3") {
            t.driver = "nope".to_string();
        }
    })?;
    let r = run(&ws.engine, "//f:root").await;
    let f = r.assert_blames("//f:l3");
    assert!(format!("{:#}", f.source).contains("nope"), "{:#}", f.source);
    Ok(())
}

/// D1.c rule 5: an error under `collect_transitive_deps`' typed
/// `StageFrame(Spec)` (the dep's own `get`) blames the dep, not the target
/// whose def was collecting it.
#[tokio::test]
async fn transitive_dep_spec_error_blames_the_dep() -> anyhow::Result<()> {
    let ws = stage_ws(
        Faults {
            fail_get: vec![addr("//f:l3")],
            ..Default::default()
        },
        DriverFaults::default(),
        |_| {},
    )?;
    let r = run(&ws.engine, "//f:l2").await;
    let f = r.assert_blames("//f:l3");
    let frame = downcast_chain_ref::<StageFrame>(&f.source)
        .unwrap_or_else(|| panic!("a StageFrame on {:#}", f.source));
    assert_eq!(frame.stage, Stage::Spec);
    assert_eq!(frame.addr.format(), "//f:l3");
    Ok(())
}

// ---------------------------------------------------------------------------
// D1.b adversarial: who a not-found blames (the e2e rows; every row is a
// unit test over `failure_fate` in result.rs)
// ---------------------------------------------------------------------------

/// `//c:y → //d:t`, which declares `//e:missing`: `//d:t`, not `//c:y`.
/// Through the engine the error reaches `//c:y` through the memoizer's
/// shared wrapper, so this is the `SharedAnyhow` row as well.
#[tokio::test]
async fn d1b_attribution_deeper_declarer() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("c", &t("y", r#"deps = ["//d:t"]"#)),
        ("d", &t("t", r#"deps = ["//e:missing"]"#)),
        ("e", &t("present", "")),
    ]);
    let r = run(&ws.engine, "//c:y").await;
    let m = missing_dep(r.assert_blames("//d:t"));
    assert_eq!(m.dep.format(), "//e:missing");
    Ok(())
}

/// `//c:y` declares `//p:ab`, which declares the missing `//p:a`: the
/// not-found names `//p:a`, so `//c:y` (whose ref is `//p:ab`) is not
/// blamed; `//p:ab`, where `//p:a` is declared, is.
#[tokio::test]
async fn d1b_attribution_near_miss_name() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("c", &t("y", r#"deps = ["//p:ab"]"#)),
        ("p", &t("ab", r#"deps = ["//p:a"]"#)),
    ]);
    let r = run(&ws.engine, "//c:y").await;
    let m = missing_dep(r.assert_blames("//p:ab"));
    assert_eq!(m.dep.format(), "//p:a");
    Ok(())
}

/// `//c:y` declares `//d:missing|out`: the ref is compared without its
/// output group, so the declarer is blamed.
#[tokio::test]
async fn d1b_attribution_output_group() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("c", &t("y", r#"deps = ["//d:missing|out"]"#)),
        ("d", D_PRESENT),
    ]);
    let r = run(&ws.engine, "//c:y").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    Ok(())
}

// ---------------------------------------------------------------------------
// Cycles, cancellation, and several culprits in one request
// ---------------------------------------------------------------------------

/// D1.c: a `CycleError` under an `InputDefFailed` is still a cycle,
/// propagated as itself, never recorded as a target's failure.
#[tokio::test]
async fn cycle_under_input_def_is_a_cycle_not_a_failure() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("root", r#"deps = ["//b:x"]"#)),
        ("b", &t("x", r#"deps = ["//c:y"]"#)),
        ("c", &t("y", r#"deps = ["//b:x"]"#)),
    ]);
    let r = run(&ws.engine, "//a:root").await;
    let err = r.err.as_ref().expect("failed");
    assert!(
        downcast_chain_ref::<CycleError>(err).is_some(),
        "a cycle: {err:#}"
    );
    assert!(r.failures.is_empty(), "nothing recorded:\n{}", r.describe());
    Ok(())
}

/// RUN-22: Ctrl-C while `//a:slowtop`'s two deps run is a cancellation:
/// the request ends cancelled and nothing is recorded, least of all
/// `//a:slowtop` blamed for itself.
#[tokio::test]
async fn cancel_during_deps_is_cancelled_not_self_blamed() -> anyhow::Result<()> {
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(|_| {
            Box::new(
                FaultProvider::new(
                    vec![
                        bash("//a:slow1", "sleep 20; echo ok > $OUT", &[]),
                        bash("//a:slow2", "sleep 20; echo ok > $OUT", &[]),
                        bash("//a:slowtop", "echo ok > $OUT", &["//a:slow1", "//a:slow2"]),
                    ],
                    Faults::default(),
                )
                .expect("provider"),
            )
        })
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BuildEvent>();
    let rs = ws.engine.new_state_with_events(false, Some(tx));
    let top = addr("//a:slowtop");
    let opts = ResultOptions::default();
    let build = Arc::clone(&ws.engine).result_addr(rs.clone(), &top, OutputMatcher::All, &opts);
    let canceller = async {
        let mut started = 0;
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            if matches!(&ev.kind, BuildEventKind::ExecuteStart { addr, .. } if addr.starts_with("//a:slow"))
            {
                started += 1;
                if started == 2 {
                    rs.ctoken().cancel();
                }
            }
            let end = matches!(&ev.kind, BuildEventKind::ResultEnd { addr, .. } if *addr == "//a:slowtop");
            events.push(ev);
            if end {
                break;
            }
        }
        events
    };
    let (res, events) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(build, canceller)
    })
    .await?;
    let err = match res {
        Ok(_) => panic!("a cancelled build cannot succeed"),
        Err(e) => e,
    };
    assert!(
        downcast_chain_ref::<CancelledError>(&err).is_some(),
        "a cancellation, not a failure: {err:#}"
    );
    let failures = rs.take_failures();
    assert!(
        failures.is_empty(),
        "nothing recorded: {:?}",
        failures.iter().map(|f| f.culprit()).collect::<Vec<_>>()
    );
    // `//a:slowtop` never names itself as its own root.
    for ev in &events {
        if let BuildEventKind::ResultEnd {
            addr, upstream_of, ..
        } = &ev.kind
        {
            assert_ne!(upstream_of.as_deref(), Some(addr.as_str()), "{ev:?}");
        }
    }
    Ok(())
}

/// RUN-03: two independent leaves fail; both are recorded, the target
/// depending on both is not.
#[tokio::test]
async fn two_failed_leaves_both_reported() -> anyhow::Result<()> {
    let ws = build_ws(&[(
        "a",
        r#"
target(name = "l1", driver = "bash", run = "exit 3", out = "o.txt")
target(name = "l2", driver = "bash", run = "exit 4", out = "o.txt")
target(name = "par", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = {"x": ":l1", "y": ":l2"})
"#,
    )]);
    let r = run(&ws.engine, "//a:par").await;
    assert_eq!(r.culprits(), ["//a:l1", "//a:l2"], "{}", r.describe());
    // stage B: the skipped root's `more: 1`, its path.
    Ok(())
}

/// RUN-04: a diamond over one failing leaf: one culprit.
#[tokio::test]
async fn diamond_shared_leaf_one_box() -> anyhow::Result<()> {
    let ws = build_ws(&[(
        "a",
        r#"
target(name = "shared", driver = "bash", run = "echo shared broke >&2; exit 2", out = "o.txt")
target(name = "left", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":shared"])
target(name = "right", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":shared"])
target(name = "top", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = {"l": ":left", "r": ":right"})
"#,
    )]);
    let r = run(&ws.engine, "//a:top").await;
    assert_eq!(r.culprits(), ["//a:shared"], "{}", r.describe());
    // stage B: `more: 0`, path through //a:left.
    Ok(())
}

/// STAGE-17: `-e //f/...` over three independent faults (a `get`, a run, a
/// `parse`): three culprits, each once, and nothing above them recorded.
#[tokio::test]
async fn three_faults_three_boxes() -> anyhow::Result<()> {
    let ws = stage_ws(
        Faults {
            fail_get: vec![addr("//f:l3")],
            ..Default::default()
        },
        DriverFaults {
            parse: vec![addr("//f:s3")],
            ..Default::default()
        },
        |ts| set_run(ts, "//f:s2", "exit 1"),
    )?;
    let r = run_matcher(&ws.engine, &Matcher::PackagePrefix(PkgBuf::from("f"))).await;
    assert!(r.err.is_some(), "the selection must fail");
    assert_eq!(
        r.culprits(),
        ["//f:l3", "//f:s2", "//f:s3"],
        "{}",
        r.describe()
    );
    Ok(())
}
