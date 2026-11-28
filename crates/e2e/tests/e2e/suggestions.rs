//! "Did you mean" (error-reporting D4, F4, I4): an error naming an unknown
//! thing suggests the nearest known one, as `DidYouMean` hints on the
//! recorded failure's diagnostic. The near-miss helper itself is unit-tested
//! in `hcore::suggest`; these pin what each error site feeds it and what the
//! record carries.

use crate::source_location::{at, fail, loc, one, t, text, triple, ws_with};
use hcore::diagnostic::{Hint, HintKind, Stage};

/// The record's hints of `kind`, as text.
fn hints(f: &hplugin::error::TargetFailure, kind: HintKind) -> Vec<String> {
    f.record
        .diagnostic
        .hints
        .iter()
        .filter(|h| h.kind == kind)
        .map(|h| h.text.clone())
        .collect()
}

/// OPT-01 through the engine: an in-process driver's unknown option is a
/// diagnostic naming the driver, the nearest option and the valid ones,
/// still pointing at the misspelled keyword.
#[tokio::test]
async fn unknown_option_suggests_nearest_option() {
    let (ws, src) = ws_with("dep = []");
    let f = fail(&ws, "//c:y").await;
    let d = &f.record.diagnostic;
    assert_eq!(d.stage, Stage::Parse, "{}", text(&f));
    assert_eq!(d.message, "unknown option `dep` for driver `bash`");
    assert_eq!(
        d.hints.first(),
        Some(&Hint::new(HintKind::DidYouMean, "deps"))
    );
    let valid = hints(&f, HintKind::ValidValues);
    assert!(
        valid
            .first()
            .is_some_and(|v| v.starts_with("valid options: ")),
        "{valid:?}"
    );
    assert_eq!(
        triple(loc(&f)),
        at("c/BUILD", &src, "dep ="),
        "{}",
        text(&f)
    );
}

/// OPT-28 / EVAL-14: a driver name no plugin registered suggests the nearest
/// registered one, and still points at the `driver` option.
#[tokio::test]
async fn unknown_driver_suggests_nearest() {
    let src = "# y\ntarget(name = \"y\", driver = \"bsh\", run = \"x\")\n";
    let ws = crate::common::Workspace::new();
    ws.write_build_file("c", src);
    let f = fail(&ws, "//c:y").await;
    assert_eq!(f.record.diagnostic.message, "driver not found: bsh");
    assert_eq!(hints(&f, HintKind::DidYouMean), vec!["bash".to_owned()]);
    assert_eq!(
        triple(loc(&f)),
        at("c/BUILD", src, "driver"),
        "{}",
        text(&f)
    );
}

/// CLI-27: a typo'd plugin option in `.hephconfig` names the nearest option
/// when the engine applies the plugin, and still lists the allowed ones.
#[tokio::test]
async fn plugin_option_typo_suggests() {
    use heph::commands::bootstrap;
    use heph::engine::{ConfigYamlExt as _, Engine, config_yaml};

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join(".hephconfig"),
        "plugins:\n  - builtin: buildfile\n    options:\n      pattern: [BUILD]\n",
    )
    .expect("write .hephconfig");
    let file = config_yaml::load_from_root(root).expect("load .hephconfig");
    let config = file.resolve(root).expect("resolve config");
    let home = config.home_dir.to_path_buf();
    let mut e = Engine::new(config).expect("engine");
    bootstrap::register_builtin_plugins(&mut e, &home).expect("builtin plugins");
    bootstrap::register_builtin_factories(&mut e, &home).expect("builtin factories");
    let err = e
        .register_plugins(&file.plugins)
        .expect_err("a typo'd option is refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown option `pattern`; did you mean `patterns`? (allowed: patterns, "),
        "{msg}"
    );
}

/// A BUILD file calling `print` with a string, an int and a list, on line 2.
const PRINTS: &str = concat!(
    "# y\n",
    "print(\"hello\", 42, [1])\n",
    r#"target(name = "y", driver = "bash", run = "echo ok > $OUT", out = "o.txt")"#,
    "\n",
);

/// EVAL-18 / U12: `print` exists, and an ordinary run (no debug logging)
/// shows nothing of it.
#[tokio::test]
async fn print_silent_in_ordinary_run() {
    let ws = crate::common::Workspace::new();
    ws.write_build_file("c", PRINTS);
    let (res, log) = crate::codegen_index_write::capture_logs(
        tracing_subscriber::filter::LevelFilter::INFO,
        ws.run("//c:y"),
    )
    .await;
    res.expect("a BUILD file calling print evaluates");
    assert!(!log.contains("hello"), "{log}");
}

/// EVAL-18 / U12: with debug logging, `print` is one debug line with the
/// call's file and line, its values joined as starlark's `print` joins them.
#[tokio::test]
async fn print_debug_line_with_file_line() {
    let ws = crate::common::Workspace::new();
    ws.write_build_file("c", PRINTS);
    let (res, log) = crate::codegen_index_write::capture_logs(
        tracing_subscriber::filter::LevelFilter::DEBUG,
        ws.run("//c:y"),
    )
    .await;
    res.expect("run");
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("hello 42 [1]")).collect();
    assert_eq!(lines.len(), 1, "{log}");
    let line = lines.first().copied().unwrap_or_default();
    assert!(line.contains("DEBUG"), "{line}");
    assert!(line.contains("site=c/BUILD:2"), "{line}");
}
/// `a`, requested by address as `heph run //p:x` asks for it, in request
/// `rs`: the failures it recorded. (A matcher is a query, which skips an
/// address that does not exist.)
async fn missing_in(
    ws: &htestkit::Workspace,
    rs: &std::sync::Arc<heph::engine::request_state::RequestState>,
    a: &str,
) -> Vec<std::sync::Arc<hplugin::error::TargetFailure>> {
    let res = std::sync::Arc::clone(&ws.engine)
        .result_addr(
            rs.clone(),
            &crate::source_location::addr(a),
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await;
    assert!(res.is_err(), "{a} must fail");
    rs.take_failures()
}

/// [`missing_in`] in a fresh request: its one failure, which blames `a`.
async fn missing(
    ws: &htestkit::Workspace,
    a: &str,
) -> std::sync::Arc<hplugin::error::TargetFailure> {
    let rs = ws.engine.new_state_with_events(false, None);
    one(missing_in(ws, &rs, a).await, a)
}

/// `c/BUILD` defining a bash target per name, in this order.
fn c_with(names: &[&str]) -> crate::common::Workspace {
    let src: Vec<String> = names.iter().map(|n| t(n, "")).collect();
    let ws = crate::common::Workspace::new();
    ws.write_build_file("c", &src.join("\n"));
    ws
}

/// GRAPH-01: a target missing from a package that exists keeps its message,
/// and suggests the nearest target as a full address, then lists the
/// package's targets, at most 5, the rest counted.
#[tokio::test]
async fn missing_target_suggests_nearest_and_lists_5() {
    let ws = c_with(&["test", "build", "lint", "fmt", "deploy", "docs", "bench"]);
    let f = missing(&ws, "//c:biuld").await;
    assert_eq!(
        f.record.diagnostic.message,
        "target not found: //c:biuld",
        "{}",
        text(&f)
    );
    assert_eq!(
        hints(&f, HintKind::DidYouMean),
        vec!["//c:build".to_owned()]
    );
    assert_eq!(
        hints(&f, HintKind::ValidValues),
        vec!["targets in //c: bench, build, deploy, docs, fmt, … (+2 more)".to_owned()]
    );
}

/// D4 on a declared dependency: the declarer's record names the nearest
/// target in the dependency's package, and lists nothing (hints only).
#[tokio::test]
async fn missing_dep_suggests_nearest_hints_only() {
    let ws = crate::common::Workspace::new();
    ws.write_build_file(
        "c",
        &format!("{}\n{}", t("build", ""), t("y", r#"deps = ["//c:biuld"]"#)),
    );
    let f = fail(&ws, "//c:y").await;
    assert!(
        f.record
            .diagnostic
            .message
            .ends_with("target not found: //c:biuld"),
        "{}",
        text(&f)
    );
    assert_eq!(
        hints(&f, HintKind::DidYouMean),
        vec!["//c:build".to_owned()]
    );
    assert!(hints(&f, HintKind::ValidValues).is_empty(), "{}", text(&f));
}

/// GRAPH-02, and the D4 adversarial row "`//libb:a`, package `//lib`
/// memoized": a package that the request's own walk shows does not exist
/// says so, and suggests the nearest package with the target name kept.
#[tokio::test]
async fn missing_package_suggests_from_memoized_packages() {
    let ws = crate::common::Workspace::new();
    ws.write_build_file("lib", &t("a", ""));
    ws.write_build_file("other", &t("a", ""));
    let rs = ws.engine.new_state_with_events(false, None);
    let all = heph::htmatcher::Matcher::PackagePrefix("".into());
    let packages = ws.engine.packages(&all, &rs).await.expect("packages");
    assert!(packages.iter().any(|p| p == "lib"), "{packages:?}");
    let f = one(missing_in(&ws, &rs, "//libb:a").await, "//libb:a");
    assert_eq!(
        f.record.diagnostic.message,
        "no package //libb",
        "{}",
        text(&f)
    );
    assert_eq!(hints(&f, HintKind::DidYouMean), vec!["//lib:a".to_owned()]);
}

/// The D4 adversarial row "`//libb:a`, nothing memoized": no walk proves
/// the package absent, so the message is the plain not-found and nothing is
/// suggested.
#[tokio::test]
async fn missing_package_nothing_memoized_no_suggestion() {
    let ws = crate::common::Workspace::new();
    ws.write_build_file("lib", &t("a", ""));
    let f = missing(&ws, "//libb:a").await;
    assert_eq!(
        f.record.diagnostic.message,
        "target not found: //libb:a",
        "{}",
        text(&f)
    );
    assert!(f.record.diagnostic.hints.is_empty(), "{}", text(&f));
}

/// I4: suggesting, for a root and for a declared dependency, never walks
/// the workspace for packages (`list_packages`), and never builds.
#[tokio::test]
async fn suggestion_never_calls_list_packages() {
    use heph::engine::fault_provider::{FaultProvider, Faults};
    use heph::pluginstatictarget::Target;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    let bash = |a: &str, deps: &[&str]| Target {
        addr: a.to_owned(),
        driver: "bash".to_owned(),
        run: Some("echo ok > $OUT".to_owned()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_owned()])]),
        deps: HashMap::from([(
            String::new(),
            deps.iter().map(|d| (*d).to_owned()).collect(),
        )]),
        ..Default::default()
    };
    let provider = FaultProvider::new(
        vec![bash("//p:build", &[]), bash("//p:y", &["//p:biuld"])],
        Faults::default(),
    )
    .expect("provider");
    let packages = provider.list_packages_calls();
    let ws = htestkit::WorkspaceBuilder::new()
        .expect("workspace")
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .build()
        .expect("build workspace");
    for a in ["//p:biuld", "//p:y"] {
        let f = missing(&ws, a).await;
        assert_eq!(
            hints(&f, HintKind::DidYouMean),
            vec!["//p:build".to_owned()],
            "{a}: {}",
            text(&f)
        );
    }
    assert_eq!(packages.load(Ordering::SeqCst), 0, "package walks");
}

/// D4: a not-found in a package whose file does not evaluate is the
/// evaluation's failure, intact, with nothing suggested.
#[tokio::test]
async fn broken_build_package_no_suggestion_error_intact() {
    let ws = crate::common::Workspace::new();
    ws.write_build_file("c", &format!("{}\nfail(\"boom\")\n", t("build", "")));
    let rs = ws.engine.new_state_with_events(false, None);
    let failures = missing_in(&ws, &rs, "//c:biuld").await;
    assert_eq!(failures.len(), 1, "one failure");
    let f = failures.first().expect("one failure");
    assert_eq!(f.record.file.as_deref(), Some("c/BUILD"), "{}", text(f));
    assert!(f.record.diagnostic.message.contains("boom"), "{}", text(f));
    assert!(f.record.diagnostic.hints.is_empty(), "{}", text(f));
}
