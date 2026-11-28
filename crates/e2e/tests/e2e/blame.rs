//! Who a failure is blamed on (error-reporting D1): the culprit a request
//! records — the target whose own spec, def or run failed, the declarer of a
//! dependency that does not exist, or a BUILD file that did not evaluate —
//! never the dependent that happened to resolve it.
//!
//! Each test reads the request's failure registry (`take_failures`), which
//! holds one record per culprit, its events, and the request-end pass over
//! its skipped targets (D1.e: each one's `root`, `path` and `more`, from the
//! request's `DepDag`). These pin the blame and the graph answers; a few
//! also read what the renderers make of them (the `run --json` records and
//! the text boxes), through `run_json`'s harness, whose own tests own the
//! record's shape.
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
use heph::engine::request_state::SkippedReport;
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use hplugin::diagnostic::Stage;
use hplugin::error::{CancelledError, Culprit, MissingDep, TargetFailure};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// What one request left behind.
struct Run {
    err: Option<anyhow::Error>,
    failures: Vec<Arc<TargetFailure>>,
    /// Every event the request emitted.
    events: Vec<BuildEvent>,
    /// The request-end pass over every skipped target (a non-cancelled
    /// `ResultEnd` naming an `upstream_of`).
    skipped: SkippedReport,
}

impl Run {
    /// The recorded failure of culprit `c` (an address or a file).
    fn failure(&self, c: &str) -> &TargetFailure {
        self.failures
            .iter()
            .find(|f| f.culprit().to_string() == c)
            .unwrap_or_else(|| panic!("no failure of {c}:\n{}", self.describe()))
    }

    /// `c`'s recorded path, formatted. Its compacted record names the same
    /// culprit, entry and path: the record is the failure's, not a
    /// placeholder, and the request-end path pass moves both.
    fn path(&self, c: &str) -> Vec<String> {
        let f = self.failure(c);
        let path: Vec<String> = f.path.iter().map(Addr::format).collect();
        let rec = &f.record;
        assert_eq!(rec.path, path, "{c}'s record path");
        assert_eq!(rec.entry, f.addr.format(), "{c}'s record entry");
        match &f.file {
            Some(file) => assert_eq!(rec.file.as_deref(), Some(&**file), "{c}'s record file"),
            None => assert_eq!(rec.addr.as_deref(), Some(c), "{c}'s record culprit"),
        }
        path
    }

    /// Skipped `a`'s `(root, path, more)` from the request-end pass.
    fn skipped(&self, a: &str) -> (String, Vec<String>, usize) {
        let rec = self
            .skipped
            .get(&addr(a))
            .unwrap_or_else(|| panic!("{a} skipped with a root:\n{}", self.describe()));
        (
            rec.root.to_string(),
            rec.path.iter().map(Addr::format).collect(),
            rec.more,
        )
    }

    /// The `ResultEnd` events of `a`.
    fn ends(&self, a: &str) -> Vec<&BuildEventKind> {
        self.events
            .iter()
            .map(|e| &e.kind)
            .filter(|k| matches!(k, BuildEventKind::ResultEnd { addr, .. } if addr == a))
            .collect()
    }

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
            .map(|f| {
                format!(
                    "{} (addr {}): {:#}",
                    f.culprit(),
                    f.addr.format(),
                    crate::common::failure_text(f)
                )
            })
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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BuildEvent>();
    let rs = engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(engine)
        .result(rs.clone(), m, OutputMatcher::All, &ResultOptions::default())
        .await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    let failures = rs.take_failures();
    let skipped_addrs: Vec<Addr> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr: a,
                upstream_of: Some(_),
                cancelled: false,
                ..
            } => Some(addr(a)),
            _ => None,
        })
        .collect();
    let skipped = rs.skipped_report(failures.iter().map(|f| &**f), &skipped_addrs);
    assert_eq!(rs.report_passes(), 1, "one request-end pass");
    drop(rs);
    // A selection's per-target errors ride the batch; the request error is
    // only for one that failed as a whole.
    let err = match res {
        Ok(batch) => batch.errors.into_iter().next().map(|(_, e)| e),
        Err(e) => Some(e),
    };
    Run {
        err,
        failures,
        events,
        skipped,
    }
}

/// `heph run <a>`, which must fail.
async fn run(engine: &Arc<Engine>, a: &str) -> Run {
    let r = run_matcher(engine, &Matcher::Addr(addr(a))).await;
    assert!(r.err.is_some(), "{a} must fail");
    r
}

/// The [`MissingDep`] a recorded failure's message says, read back from its
/// text (`{declarer} depends on {dep}, which does not exist (`{origin}`, …`),
/// or a panic. The record holds no error chain (D7b): what a user is told is
/// the message, so that is where the facts must be.
fn missing_dep(f: &TargetFailure) -> MissingDep {
    let text = crate::common::failure_text(f);
    let parse = || -> Option<MissingDep> {
        let (head, rest) = text.split_once(" depends on ")?;
        let declarer = head.rsplit(' ').next()?;
        let (dep, rest) = rest.split_once(", which does not exist (`")?;
        let (origin, _) = rest.split_once('`')?;
        Some(MissingDep {
            dep: addr(dep),
            declarer: addr(declarer),
            origin_id: origin.to_owned(),
        })
    };
    parse().unwrap_or_else(|| panic!("a MissingDep on {text}"))
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    // The path ends at the address that led into the file.
    assert_eq!(r.path("d/BUILD"), ["//a:t", "//b:t", "//c:t", "//d:t"]);
    // A skipped end names that address, with the file beside it: never a
    // path in an address field.
    let skipped: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                upstream_of: Some(of),
                upstream_file,
                path,
                ..
            } => Some((of.as_str(), upstream_file.as_deref(), path.len())),
            _ => None,
        })
        .collect();
    assert!(!skipped.is_empty(), "//a:t's end is skipped");
    for s in skipped {
        assert_eq!(s, ("//d:t", Some("d/BUILD"), 0));
    }
    assert_eq!(r.skipped("//a:t").0, "d/BUILD");

    // What the renderers make of it: the file's JSON record has no address
    // and names the file, with the whole path; `//a:t` is skipped because
    // of it; the text run prints one box, the file's, reached through the
    // chain.
    let json =
        crate::run_json::run_json(&ws.engine, &Matcher::Addr(addr("//a:t")), &["--json"], None)
            .await;
    assert!(json.closed, "{:?}", json.entries);
    assert_eq!(json.entries.len(), 2, "{:?}", json.entries);
    let file = &json.entries[0];
    assert!(
        file["addr"].is_null(),
        "a path never goes in `addr`: {file}"
    );
    assert_eq!(file["file"], "d/BUILD", "{file}");
    assert_eq!(file["outcome"], "failed", "{file}");
    assert_eq!(file["root_file"], "d/BUILD", "{file}");
    assert!(file["root"].is_null(), "{file}");
    assert_eq!(file["selected"], false, "{file}");
    assert_eq!(
        file["path"],
        serde_json::json!(["//a:t", "//b:t", "//c:t", "//d:t"]),
        "{file}"
    );
    let top = json.entry("//a:t");
    assert_eq!(top["outcome"], "skipped", "{top}");
    assert_eq!(top["root_file"], "d/BUILD", "{top}");
    assert!(top["root"].is_null(), "{top}");
    assert_eq!(top["path"], file["path"], "{top}");
    assert_eq!(json.box_count(), 1, "{}", json.boxes);
    assert!(json.boxes.contains("d/BUILD"), "{}", json.boxes);
    assert!(
        json.boxes.contains("reached by: //a:t → //b:t → //c:t"),
        "{}",
        json.boxes
    );
    Ok(())
}

/// U2 / D1.d: two requested targets of one broken `d/BUILD` are one
/// failure, the file's.
#[tokio::test]
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
    // Entered through the first requested target the file holds.
    assert_eq!(r.failures[0].addr.format(), "//d:t1", "{}", r.describe());
    Ok(())
}

/// CQ-M1: the selection's walk fails on `d/BUILD`, whose loaded
/// `tools/defs.star` is the broken file. The record is entered through the
/// requested target of the package the walk was scanning (`//d:t`), not one
/// guessed from the broken file's directory, which holds none of them.
#[tokio::test]
async fn walk_into_broken_loaded_file_enters_through_the_scanned_package() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("t", "")),
        (
            "d",
            &format!("load(\"//tools/defs.star\", \"x\")\n{}\n", t("t", "")),
        ),
    ]);
    ws.write_file("tools/defs.star", "x = 1\nfail(\"boom\")\n");
    let r = run_matcher(
        &ws.engine,
        &Matcher::Or(vec![
            Matcher::Addr(addr("//a:t")),
            Matcher::Addr(addr("//d:t")),
        ]),
    )
    .await;
    let err = r.err.as_ref().expect("the selection must fail");
    assert_eq!(
        r.culprits(),
        ["tools/defs.star"],
        "{}\nerr: {err:#}",
        r.describe()
    );
    assert_eq!(r.failures[0].addr.format(), "//d:t", "{}", r.describe());
    assert_eq!(r.path("tools/defs.star"), ["//d:t"]);
    Ok(())
}

/// CQ round 2: the walk of `-e '//a:x || //d/...'` fails on `d/sub/BUILD`,
/// a package no named address is in. `//a:x` did not lead into the file, so
/// it is neither requested for it nor its entry: the error is returned as a
/// pure package selection's is, and `//a:x` is blamed for nothing.
#[tokio::test]
async fn walk_into_broken_package_no_named_address_is_in_blames_none() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("x", "")),
        ("d/sub", "target(name = \"t\"\nbroken(\n"),
    ]);
    let r = run_matcher(
        &ws.engine,
        &Matcher::Or(vec![
            Matcher::Addr(addr("//a:x")),
            Matcher::PackagePrefix(PkgBuf::from("d")),
        ]),
    )
    .await;
    let err = r.err.as_ref().expect("the selection must fail");
    assert!(
        r.failures.iter().all(|f| f.addr.format() != "//a:x"),
        "//a:x is no file's entry:\n{}\nerr: {err:#}",
        r.describe()
    );
    let entered: Vec<&str> = r
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::FileFailed { entry, .. } => Some(entry.as_str()),
            _ => None,
        })
        .collect();
    assert!(!entered.contains(&"//a:x"), "{entered:?}");
    // The same as the package alone.
    let pure = run_matcher(&ws.engine, &Matcher::PackagePrefix(PkgBuf::from("d"))).await;
    assert_eq!(r.culprits(), pure.culprits(), "{}", r.describe());
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
    assert_eq!(r.path("//c:y"), ["//a:root", "//b:x", "//c:y"]);
    Ok(())
}

/// PR 1's interim text: before targets carry a source location, the
/// `MissingDep` names the declarer's BUILD file from its package.
#[tokio::test]
async fn missing_dep_interim_text_names_build_file() -> anyhow::Result<()> {
    let ws = chain_to_c(r#"deps = ["//d:missing"]"#, &[("d", D_PRESENT)]);
    let r = run(&ws.engine, "//a:root").await;
    let text = crate::common::failure_text(r.assert_blames("//c:y"));
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
async fn missing_transitive_dep_blames_declarer() -> anyhow::Result<()> {
    let ws = chain_to_c(
        r#"transitive = {"deps": ["//d:missing"]}"#,
        &[("d", D_PRESENT)],
    );
    let r = run(&ws.engine, "//a:root").await;
    let m = missing_dep(r.assert_blames("//c:y"));
    assert_eq!(m.dep.format(), "//d:missing");
    assert_eq!(m.declarer.format(), "//c:y");
    assert_eq!(m.origin_id, "transitive.deps");
    assert_not_blamed(&r, &["//a:root", "//b:x"]);
    Ok(())
}

/// CQ-M2: `//c:y` builds, and only its consumer's merge of its
/// `transitive` finds the missing dep, so `//c:y` is blamed after its own
/// scope ended. It has one `ResultStart`: the claim's winner does not
/// reopen a row that already closed. The failing `ResultEnd` still comes,
/// last, for a consumer that reads the culprit from it.
#[tokio::test]
async fn culprit_that_already_ended_gets_no_second_start() -> anyhow::Result<()> {
    let ws = chain_to_c(
        r#"transitive = {"deps": ["//d:missing"]}"#,
        &[("d", D_PRESENT)],
    );
    let r = run(&ws.engine, "//a:root").await;
    r.assert_blames("//c:y");
    let of_c: Vec<&str> = r
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultStart { addr } if addr == "//c:y" => Some("start"),
            BuildEventKind::ResultEnd { addr, error, .. } if addr == "//c:y" => {
                Some(if error.is_some() { "failed" } else { "ok" })
            }
            _ => None,
        })
        .collect();
    assert_eq!(of_c, ["start", "ok", "failed"], "{:#?}", r.events);
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
    let text = crate::common::failure_text(f);
    assert!(text.contains("enviroment"), "{text}");
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
    let text = crate::common::failure_text(f);
    assert!(text.contains("injected error at parse //f:l3"), "{text}");
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
    let text = crate::common::failure_text(f);
    assert!(
        text.contains("injected error at apply_transitive //f:l3"),
        "{text}"
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
    let text = crate::common::failure_text(f);
    assert!(text.contains("nope"), "{text}");
    Ok(())
}

/// D1.c rule 5: an error under `collect_transitive_deps`' typed
/// `get spec:` `StageFrame` (the dep's own `get`) blames the dep, not the
/// target whose def was collecting it.
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
    // The innermost stage frame (`get_spec //f:l3`) is the record's stage.
    assert_eq!(f.record.diagnostic.stage, Stage::Spec, "{:?}", f.record);
    assert_eq!(f.record.addr.as_deref(), Some("//f:l3"));
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
/// propagated through the cycle's frames as itself: no target in it is
/// blamed for an input's def. It is reported once (D8, GRAPH-07), as the
/// requested target's own failure.
#[tokio::test]
async fn cycle_under_input_def_is_a_cycle_not_a_failure() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("root", r#"deps = ["//b:x"]"#)),
        ("b", &t("x", r#"deps = ["//c:y"]"#)),
        ("c", &t("y", r#"deps = ["//b:x"]"#)),
    ]);
    let r = run(&ws.engine, "//a:root").await;
    let [f] = r.failures.as_slice() else {
        panic!("one record:\n{}", r.describe());
    };
    assert_eq!(f.culprit().to_string(), "//a:root", "{}", r.describe());
    let text = crate::common::failure_text(f);
    assert!(text.contains("cyclic dependency"), "a cycle: {text}");
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    assert_eq!(r.path("//a:l1"), ["//a:par", "//a:l1"]);
    assert_eq!(r.path("//a:l2"), ["//a:par", "//a:l2"]);
    // Both one hop away: the smaller is the root, the other counted.
    assert_eq!(
        r.skipped("//a:par"),
        ("//a:l1".into(), vec!["//a:par".into(), "//a:l1".into()], 1)
    );

    // Each culprit's JSON `error` is one non-empty line; the skipped target
    // has none, and counts the other culprit in `more`.
    let json = crate::run_json::run_json(
        &ws.engine,
        &Matcher::Addr(addr("//a:par")),
        &["--json"],
        None,
    )
    .await;
    assert!(json.closed, "{:?}", json.entries);
    for leaf in ["//a:l1", "//a:l2"] {
        let e = json.entry(leaf);
        assert_eq!(e["outcome"], "failed", "{e}");
        assert!(
            e["error"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && !s.contains('\n')),
            "one line: {e}"
        );
    }
    let par = json.entry("//a:par");
    assert_eq!(par["outcome"], "skipped", "{par}");
    assert!(par["error"].is_null(), "{par}");
    assert_eq!(par["root"], "//a:l1", "{par}");
    assert_eq!(par["more"], 1, "{par}");
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
    // Two equal paths: through the smaller address.
    assert_eq!(r.path("//a:shared"), ["//a:top", "//a:left", "//a:shared"]);
    assert_eq!(
        r.skipped("//a:top"),
        (
            "//a:shared".into(),
            vec!["//a:top".into(), "//a:left".into(), "//a:shared".into()],
            0
        )
    );
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

// ---------------------------------------------------------------------------
// Every failure reported (D1.f, STAGE-16)
// ---------------------------------------------------------------------------

/// STAGE-16: three of `//f:root`'s four inputs fail to resolve, one deep in
/// its subtree (`//f:l3`'s `get`) and two at the input (`parse` of `//f:s2`
/// and `//f:s3`). Each is its own culprit, the root is not one, and the
/// healthy sibling `//f:s1` still ran.
#[tokio::test]
async fn independent_faults_all_reported_siblings_run() -> anyhow::Result<()> {
    let ws = stage_ws(
        Faults {
            fail_get: vec![addr("//f:l3")],
            ..Default::default()
        },
        DriverFaults {
            parse: vec![addr("//f:s2"), addr("//f:s3")],
            ..Default::default()
        },
        |_| {},
    )?;
    let r = run(&ws.engine, "//f:root").await;
    assert_eq!(
        r.culprits(),
        ["//f:l3", "//f:s2", "//f:s3"],
        "{}",
        r.describe()
    );
    assert_not_blamed(&r, &["//f:root", "//f:l1", "//f:s1"]);
    let s1_ran = r.ends("//f:s1").iter().any(|k| {
        matches!(
            k,
            BuildEventKind::ResultEnd {
                error: None,
                cancelled: false,
                ..
            }
        )
    });
    assert!(s1_ran, "the healthy sibling ran: {:?}", r.ends("//f:s1"));
    Ok(())
}

/// D1.f: two groups share a nested group whose member is refused (an
/// entrypoint). The walk of the second group meets the nested group again;
/// it must report the refusal, not a "circular group dependency" left
/// behind by the first walk's error.
#[tokio::test]
async fn group_error_is_not_a_false_cycle() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("x", r#"deps = {"p": "//g:g1", "q": "//g:g2"}"#)),
        (
            "g",
            r#"
target(name = "g1", driver = "group", deps = [":h"])
target(name = "g2", driver = "group", deps = [":h"])
target(name = "h", driver = "group", deps = ["//e:ep"])
"#,
        ),
        (
            "e",
            r#"target(name = "ep", driver = "bash", entrypoint = True, run = "true")"#,
        ),
    ]);
    let r = run(&ws.engine, "//a:x").await;
    let text = r.describe();
    assert!(!text.contains("circular group dependency"), "{text}");
    assert!(text.contains("is an entrypoint"), "{text}");
    Ok(())
}

/// D1.f: two inputs of `//top:t` reach one broken `e/BUILD` through two
/// different targets (`//d:x → //e:a`, `//d:y → //e:b`). Under keep-going
/// the members blame the same file through different entries: one culprit,
/// the file, and the classification terminates (it once regrouped the
/// members forever and overflowed the stack).
#[tokio::test]
async fn two_entries_into_one_broken_file_blame_the_file() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("top", &t("t", r#"deps = ["//d:x", "//d:y"]"#)),
        (
            "d",
            &format!(
                "{}\n{}\n",
                t("x", r#"deps = ["//e:a"]"#),
                t("y", r#"deps = ["//e:b"]"#)
            ),
        ),
        (
            "e",
            &format!("{}\n{}\nfail(\"boom\")\n", t("a", ""), t("b", "")),
        ),
    ]);
    let r = run(&ws.engine, "//top:t").await;
    assert_eq!(r.culprits(), ["e/BUILD"], "{}", r.describe());
    assert_not_blamed(&r, &["//top:t", "//d:x", "//d:y"]);
    // The record keeps the first member's entry, and its path ends there.
    let f = r.failure("e/BUILD");
    assert_eq!(f.addr.format(), "//e:a", "{}", r.describe());
    assert_eq!(r.path("e/BUILD"), ["//top:t", "//d:x", "//e:a"]);
    assert_eq!(r.skipped("//top:t").0, "e/BUILD");
    assert_file_diagnostic_once(f);
    Ok(())
}

/// CQ round 2: a file culprit reached through several entries records its
/// first member only, so the box and `--json`'s `error` say the file's
/// diagnostic once, not once per entry under the consumer's frame.
fn assert_file_diagnostic_once(f: &TargetFailure) {
    let text = crate::common::failure_text(f);
    assert_eq!(text.matches("fail: boom").count(), 1, "{text}");
    // Not an aggregate: no `N errors` frame, no causes.
    assert!(f.record.causes.is_empty(), "{text}");
}

/// D1.f: `//top:t`'s two inputs live in one `d/BUILD` that fails at
/// evaluation. Keep-going blames the file, as fail-fast does, never the
/// consumer that resolved both inputs.
#[tokio::test]
async fn two_inputs_in_one_broken_file_blame_the_file() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("top", &t("t", r#"deps = ["//d:x", "//d:y"]"#)),
        (
            "d",
            &format!("{}\n{}\nfail(\"boom\")\n", t("x", ""), t("y", "")),
        ),
    ]);
    let r = run(&ws.engine, "//top:t").await;
    assert_eq!(r.culprits(), ["d/BUILD"], "{}", r.describe());
    assert_not_blamed(&r, &["//top:t"]);
    let f = r.failure("d/BUILD");
    assert_eq!(f.addr.format(), "//d:x", "{}", r.describe());
    assert_eq!(r.path("d/BUILD"), ["//top:t", "//d:x"]);
    assert_eq!(r.skipped("//top:t").0, "d/BUILD");
    assert_file_diagnostic_once(f);
    Ok(())
}

// ---------------------------------------------------------------------------
// Paths and counts (D1.e, P18, U17)
// ---------------------------------------------------------------------------

/// GRAPH-08: a diamond over a declarer naming a missing dep: one culprit,
/// and the same path through the smaller address on every run, whatever
/// order the scheduler resolved the two sides in.
#[tokio::test]
async fn diamond_missing_dep_same_path_20_runs() -> anyhow::Result<()> {
    let build = r#"
target(name = "shared", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":missing"])
target(name = "left", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":shared"])
target(name = "right", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":shared"])
target(name = "top", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = {"r": ":right", "l": ":left"})
"#;
    for i in 0..20 {
        let ws = build_ws(&[("a", build)]);
        let r = run(&ws.engine, "//a:top").await;
        assert_eq!(r.culprits(), ["//a:shared"], "run {i}: {}", r.describe());
        assert_eq!(
            r.path("//a:shared"),
            ["//a:top", "//a:left", "//a:shared"],
            "run {i}"
        );
    }
    Ok(())
}

/// D1.e ties: `//a:top` reaches two culprits two hops away, declared the
/// larger first: the root is the smaller address, the other one counted.
#[tokio::test]
async fn two_roots_tie_broken_by_addr() -> anyhow::Result<()> {
    let ws = build_ws(&[(
        "a",
        r#"
target(name = "c2", driver = "bash", run = "exit 2", out = "o.txt")
target(name = "c1", driver = "bash", run = "exit 1", out = "o.txt")
target(name = "m2", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":c2"])
target(name = "m1", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":c1"])
target(name = "top", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = {"x": ":m2", "y": ":m1"})
"#,
    )]);
    let r = run(&ws.engine, "//a:top").await;
    assert_eq!(r.culprits(), ["//a:c1", "//a:c2"], "{}", r.describe());
    assert_eq!(
        r.skipped("//a:top"),
        (
            "//a:c1".into(),
            vec!["//a:top".into(), "//a:m1".into(), "//a:c1".into()],
            1
        )
    );
    assert_eq!(r.skipped("//a:m2").2, 0, "//a:m2 reaches only //a:c2");
    Ok(())
}

/// D1.e: two requested targets one hop above the culprit, the larger
/// named first: the path starts at the smaller.
#[tokio::test]
async fn several_requested_path_starts_at_smallest_addr() -> anyhow::Result<()> {
    let ws = build_ws(&[(
        "a",
        r#"
target(name = "c", driver = "bash", run = "exit 1", out = "o.txt")
target(name = "r2", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":c"])
target(name = "r1", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":c"])
"#,
    )]);
    let r = run_matcher(
        &ws.engine,
        &Matcher::Or(vec![
            Matcher::Addr(addr("//a:r2")),
            Matcher::Addr(addr("//a:r1")),
        ]),
    )
    .await;
    assert_eq!(r.culprits(), ["//a:c"], "{}", r.describe());
    assert_eq!(r.path("//a:c"), ["//a:r1", "//a:c"]);
    Ok(())
}

/// D1.e / P18: on the event stream a skipped end names its root and never
/// a path (the stream would grow by skipped × depth); the culprit's own end
/// carries its path.
#[tokio::test]
async fn skipped_result_end_carries_root_not_path() -> anyhow::Result<()> {
    let ws = build_ws(&[(
        "a",
        r#"
target(name = "c", driver = "bash", run = "exit 1", out = "o.txt")
target(name = "mid", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":c"])
target(name = "top", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":mid"])
"#,
    )]);
    let r = run(&ws.engine, "//a:top").await;
    assert_eq!(r.culprits(), ["//a:c"], "{}", r.describe());
    for skipped in ["//a:top", "//a:mid"] {
        let ends = r.ends(skipped);
        let [
            end @ BuildEventKind::ResultEnd {
                upstream_of,
                upstream_file,
                path,
                ..
            },
        ] = ends.as_slice()
        else {
            panic!("one end for {skipped}: {ends:?}")
        };
        assert_eq!(upstream_of.as_deref(), Some("//a:c"), "{skipped}");
        assert_eq!(*upstream_file, None, "{skipped}");
        assert!(path.is_empty(), "{skipped} carries no path: {path:?}");
        let wire = serde_json::to_string(end)?;
        assert!(!wire.contains("\"path\""), "no `path` on the wire: {wire}");
    }
    let culprit = r.ends("//a:c");
    let [
        BuildEventKind::ResultEnd {
            path, upstream_of, ..
        },
    ] = culprit.as_slice()
    else {
        panic!("one end for //a:c: {culprit:?}")
    };
    assert_eq!(*upstream_of, None);
    assert_eq!(path, &["//a:top", "//a:mid", "//a:c"]);
    assert_eq!(r.path("//a:c"), ["//a:top", "//a:mid", "//a:c"]);
    Ok(())
}

/// D1.e / P18: 20k targets, ~100k edges, 20k skipped under 5 culprits. The
/// graph is built straight into a request's `DepDag` (`add_dep_for_test`),
/// as resolution would commit it, rather than by running 20k targets: the
/// cost under test is the request-end pass, not the build. One pass for the
/// skipped records and one for the culprits' paths, counted; the clock only
/// catches a hang. The pure chain, where the output itself is quadratic,
/// is `run_json`'s `finish_pure_chain_output_size`.
#[tokio::test]
async fn failure_report_cost_20k() -> anyhow::Result<()> {
    const N: usize = 20_000;
    let ws = Workspace::new();
    let rs = ws.engine.new_state_with_fail_fast(false);
    let nodes: Vec<Addr> = (0..N).map(|i| addr(&format!("//p:n{i:05}"))).collect();
    let culprits: Vec<Addr> = (0..5).map(|k| addr(&format!("//c:c{k}"))).collect();
    // The spine first, so every later edge runs forward in the order and
    // the cycle check stays O(1): building the graph is not what is timed.
    for w in nodes.windows(2) {
        if let [from, to] = w {
            rs.add_dep_for_test(from, to)?;
        }
    }
    let mut edges = N - 1;
    for (i, from) in nodes.iter().enumerate() {
        for step in [3, 7, 131, 1031, 7919] {
            if let Some(to) = nodes.get(i + step) {
                rs.add_dep_for_test(from, to)?;
                edges += 1;
            }
        }
        if i % 4000 == 3999
            && let Some(c) = culprits.get(i / 4000)
        {
            rs.add_dep_for_test(from, c)?;
            edges += 1;
        }
    }
    assert!(edges > 95_000, "~100k edges, got {edges}");
    for r in nodes.iter().take(100) {
        rs.mark_requested(r);
    }
    for c in &culprits {
        rs.record_failure(c.clone(), Arc::new(TargetFailure::new(c.clone())));
    }
    let started = std::time::Instant::now();
    let failures = rs.take_failures();
    assert_eq!(rs.path_passes(), 1, "one pass for every culprit's path");
    let report = rs.skipped_report(failures.iter().map(|f| &**f), &nodes);
    let took = started.elapsed();
    assert_eq!(rs.report_passes(), 1, "one request-end pass");
    assert_eq!(report.len(), N, "every skipped target has a root");
    assert_eq!(report.bitset_words(), N + 5, "5 culprits: one word a node");
    assert!(
        took < std::time::Duration::from_secs(60),
        "a hang, not a cost: {took:?}"
    );
    // The spine reaches every culprit from the top; the last stretch only
    // the last culprit.
    let top = report.get(&nodes[0]).expect("top");
    assert_eq!(top.more, 4);
    let tail = report.get(&nodes[N - 1]).expect("tail");
    assert_eq!((tail.root.to_string().as_str(), tail.more), ("//c:c4", 0));
    Ok(())
}

/// D1.a: the culprit record of a 500-deep chain is the size of a 5-deep
/// one: each level above the culprit adds nothing to the recorded error.
#[tokio::test]
async fn deep_chain_error_size_is_constant() -> anyhow::Result<()> {
    async fn recorded(depth: usize) -> (usize, usize, Vec<String>) {
        let mut build = String::from(
            "target(name = \"leaf\", driver = \"bash\", run = \"echo ok > $OUT\", out = \"o.txt\", deps = [\":missing\"])\n",
        );
        for i in 0..depth {
            // Fixed-width names, so the two chains' addresses are the same
            // length and only depth differs.
            let dep = if i + 1 == depth {
                "leaf".to_string()
            } else {
                format!("n{:03}", i + 1)
            };
            build.push_str(&format!(
                "target(name = \"n{i:03}\", driver = \"bash\", run = \"echo ok > $OUT\", out = \"o.txt\", deps = [\":{dep}\"])\n"
            ));
        }
        let ws = build_ws(&[("a", &build)]);
        let r = run(&ws.engine, "//a:n000").await;
        let f = r.assert_blames("//a:leaf");
        (
            crate::common::failure_text(f).len(),
            f.record.text_len(),
            r.path("//a:leaf"),
        )
    }
    let (shallow_len, shallow_record, shallow_path) = recorded(5).await;
    let (deep_len, deep_record, deep_path) = recorded(500).await;
    assert_eq!(deep_len, shallow_len, "message size");
    assert_eq!(deep_record, shallow_record, "record size");
    assert_eq!(shallow_path.len(), 6);
    assert_eq!(deep_path.len(), 501, "the path is the full chain");
    Ok(())
}

/// I5: the path and the roots live for one request. On the same engine, a
/// second request whose graph no longer holds the culprit (the engine has no
/// in-process way to re-read a fixed BUILD file mid-test, so the "fix" is
/// asking for the healthy side) reports nothing from the first request.
#[tokio::test]
async fn second_request_after_fix_has_no_stale_path() -> anyhow::Result<()> {
    let ws = build_ws(&[(
        "a",
        r#"
target(name = "c", driver = "bash", run = "exit 1", out = "o.txt")
target(name = "ok", driver = "bash", run = "echo ok > $OUT", out = "o.txt")
target(name = "top", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":c", ":ok"])
target(name = "fixed", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":ok"])
"#,
    )]);
    let first = run(&ws.engine, "//a:top").await;
    assert_eq!(first.path("//a:c"), ["//a:top", "//a:c"]);
    assert_eq!(first.skipped("//a:top").0, "//a:c");

    let second = run_matcher(&ws.engine, &Matcher::Addr(addr("//a:fixed"))).await;
    assert!(second.err.is_none(), "fixed: {:?}", second.err);
    assert!(second.failures.is_empty(), "{}", second.describe());
    assert!(second.skipped.is_empty());
    for ev in &second.events {
        if let BuildEventKind::ResultEnd {
            addr,
            upstream_of,
            path,
            ..
        } = &ev.kind
        {
            assert_eq!(*upstream_of, None, "{addr}");
            assert!(path.is_empty(), "{addr}: {path:?}");
        }
    }
    Ok(())
}

/// Every `ResultStart` of `r` has a `ResultEnd` of the same address.
fn assert_every_start_ends(r: &Run) {
    let mut open: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for e in &r.events {
        match &e.kind {
            BuildEventKind::ResultStart { addr } => *open.entry(addr).or_default() += 1,
            BuildEventKind::ResultEnd { addr, .. } => *open.entry(addr).or_default() -= 1,
            _ => {}
        }
    }
    open.retain(|_, n| *n != 0);
    assert!(open.is_empty(), "unbalanced scopes: {open:?}");
}

/// The announcing failed ends of `r`: what a renderer boxes and counts.
fn announcing_ends(r: &Run) -> Vec<&str> {
    r.events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr,
                error: Some(_),
                upstream_of: None,
                cancelled: false,
                duplicate: false,
                ..
            } => Some(addr.as_str()),
            _ => None,
        })
        .collect()
}

/// D1.d / P24: `//a:x` fails in two scopes (requested itself, and as
/// `//b:y`'s dep, a second memo cell): both end, one announcing.
#[tokio::test]
async fn losing_scope_ends_non_announcing() -> anyhow::Result<()> {
    let ws = build_ws(&[
        (
            "a",
            r#"target(name = "x", driver = "bash", run = "exit 1", out = "o.txt")"#,
        ),
        ("b", &t("y", r#"deps = ["//a:x"]"#)),
    ]);
    let r = run_matcher(
        &ws.engine,
        &Matcher::Or(vec![
            Matcher::Addr(addr("//a:x")),
            Matcher::Addr(addr("//b:y")),
        ]),
    )
    .await;
    assert_every_start_ends(&r);
    assert_eq!(r.culprits(), ["//a:x"], "{}", r.describe());
    assert_eq!(announcing_ends(&r), ["//a:x"], "one box: {:#?}", r.events);
    let own: Vec<_> = r
        .ends("//a:x")
        .into_iter()
        .filter(|k| matches!(k, BuildEventKind::ResultEnd { error: Some(_), .. }))
        .collect();
    // Two scopes of `//a:x` ended failed; the losing one is flagged.
    assert_eq!(own.len(), 2, "{:#?}", r.events);
    assert!(
        own.iter().any(|k| matches!(
            k,
            BuildEventKind::ResultEnd {
                duplicate: true,
                ..
            }
        )),
        "{:#?}",
        r.events
    );
    Ok(())
}

/// D1.d / P15: a broken BUILD file rides its own `FileFailed`, once, and
/// no path ever lands in `ResultEnd.addr` / `upstream_of`.
#[tokio::test]
async fn file_culprit_rides_its_own_event_kind() -> anyhow::Result<()> {
    let ws = build_ws(&[
        ("a", &t("t", r#"deps = ["//b:t"]"#)),
        ("b", &t("t", r#"deps = ["//d:t"]"#)),
        ("d", "target(name = \"t\"\nbroken(\n"),
    ]);
    let r = run(&ws.engine, "//a:t").await;
    assert_every_start_ends(&r);
    let files: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::FileFailed {
                file, entry, path, ..
            } => Some((file.as_str(), entry.as_str(), path.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        files,
        [(
            "d/BUILD",
            "//d:t",
            vec!["//a:t".to_owned(), "//b:t".to_owned(), "//d:t".to_owned()]
        )],
        "{:#?}",
        r.events
    );
    assert!(
        announcing_ends(&r).is_empty(),
        "the file's box is its own kind"
    );
    for e in &r.events {
        if let BuildEventKind::ResultEnd {
            addr, upstream_of, ..
        } = &e.kind
        {
            for a in std::iter::once(addr).chain(upstream_of) {
                assert!(
                    a.starts_with("//") && !a.contains("BUILD"),
                    "a path in an addr: {a}"
                );
            }
        }
    }
    Ok(())
}
