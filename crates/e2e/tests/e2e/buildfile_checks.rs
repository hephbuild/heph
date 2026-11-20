//! The BUILD-file check targets end to end (docs/BUILDFILE_CHECKS.md):
//! `build-lint-check`, `build-format-check` and `build-format`, listed in
//! every package and run by the buildfile plugin's own drivers.
//!
//! Every engine here is built the way `heph` builds one: the workspace's
//! `.hephconfig` loaded and resolved, the binary's builtin plugins and
//! factories registered (`commands::bootstrap`), and the `plugins:` entries
//! applied. So the buildfile plugin is the shipped one, its options decoded
//! from the file, and a check runs in a real sandbox: what it reads is what
//! its def staged, its failure text lands in the target's failure log, and
//! `build-format` writes back through the engine's `in_place` path.
//!
//! Each run gets a fresh engine, as the next `heph` invocation would: specs
//! and defs are memoized per engine, so an edit or a flipped option is only
//! seen by a new one. The on-disk cache is shared between them.

use futures::TryStreamExt as _;
use hcore::events::CacheOutcomeKind;
use hcore::htvalue::Value;
use heph::commands::bootstrap;
use heph::commands::inspect::failure::failure_view;
use heph::engine::config_yaml;
use heph::engine::event::BuildEventKind;
use heph::engine::{ConfigYamlExt as _, Discovery, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::pluginbuildfile::ShadowedChecks;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::Arc;

/// The documented CI selector (docs/BUILDFILE_CHECKS.md), the `-e` argument of
/// `heph validate && heph run -e '…'`.
const CI_SELECTOR: &str = "label(lint-check) || label(format-check)";

const FOOTER_ANNOTATIONS: &str = "annotations are required by requireTypeAnnotations in .hephconfig (plugins: buildfile, options)";

/// A workspace on disk and the shadow record its buildfile plugin fills.
struct Ws {
    dir: tempfile::TempDir,
    shadowed: ShadowedChecks,
}

impl Ws {
    /// A workspace holding `files`, with the buildfile plugin configured by
    /// `options` (YAML mapping lines, `""` for none).
    fn new(files: &[(&str, &str)], options: &str) -> Self {
        let ws = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            shadowed: ShadowedChecks::default(),
        };
        for (path, content) in files {
            ws.write(path, content);
        }
        ws.set_options(options);
        ws
    }

    fn write(&self, rel: &str, content: &str) {
        let abs = self.dir.path().join(rel);
        std::fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
        std::fs::write(abs, content).expect("write");
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(rel)).expect("read")
    }

    /// What a write-back would change even with the same bytes: the mtime,
    /// and the inode a replace-by-rename gives the file.
    fn stamp(&self, rel: &str) -> (std::time::SystemTime, u64) {
        let meta = std::fs::metadata(self.dir.path().join(rel)).expect("stat");
        (meta.modified().expect("mtime"), meta.ino())
    }

    fn mode(&self, rel: &str) -> u32 {
        std::fs::metadata(self.dir.path().join(rel))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    }

    fn set_mode(&self, rel: &str, mode: u32) {
        std::fs::set_permissions(
            self.dir.path().join(rel),
            std::fs::Permissions::from_mode(mode),
        )
        .expect("chmod");
    }

    /// Rewrite `.hephconfig` with the buildfile plugin's `options`.
    fn set_options(&self, options: &str) {
        let mut config = String::from("plugins:\n  - builtin: buildfile\n");
        if !options.is_empty() {
            config.push_str("    options:\n");
            for line in options.lines() {
                config.push_str("      ");
                config.push_str(line);
                config.push('\n');
            }
        }
        self.write(".hephconfig", &config);
    }

    /// The engine the next `heph` invocation over this workspace builds.
    fn engine(&self) -> Arc<Engine> {
        self.engine_with(|_| {})
    }

    /// [`Self::engine`], with `extra` registering more before it is shared.
    fn engine_with(&self, extra: impl FnOnce(&mut Engine)) -> Arc<Engine> {
        let root = self.dir.path();
        let file = config_yaml::load_from_root(root).expect("load .hephconfig");
        let config = file.resolve(root).expect("resolve config");
        let home = config.home_dir.to_path_buf();
        let mut e = Engine::new(config).expect("engine");
        bootstrap::register_builtin_plugins(&mut e, &home).expect("builtin plugins");
        bootstrap::register_builtin_factories_with(&mut e, &home, self.shadowed.clone())
            .expect("builtin factories");
        e.register_plugins(&file.plugins).expect("plugins:");
        extra(&mut e);
        let e = Arc::new(e);
        e.install_exec_runner_host();
        e
    }
}

/// What one `heph run` did: the addrs that failed (with their error), the
/// ones that built, and each cacheable target's cache outcome.
#[derive(Debug)]
struct Run {
    failed: BTreeMap<String, String>,
    ok: BTreeSet<String>,
    outcomes: BTreeMap<String, CacheOutcomeKind>,
}

impl Run {
    fn outcome(&self, addr: &str) -> Option<CacheOutcomeKind> {
        self.outcomes.get(addr).copied()
    }
}

/// `heph run <m>` on a fresh engine over `ws`, keeping going past failures.
async fn run(ws: &Ws, m: &Matcher) -> Run {
    try_run(ws, m).await.expect("the selection resolves")
}

/// [`run`], or why the selection itself did not resolve.
async fn try_run(ws: &Ws, m: &Matcher) -> anyhow::Result<Run> {
    let engine = ws.engine();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let batch = Arc::clone(&engine)
        .result(
            Arc::clone(&rs),
            m,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let batch = match batch {
        Ok(batch) => batch,
        Err(e) => {
            crate::common::drain(rs).await;
            return Err(e);
        }
    };
    let failed = batch
        .errors
        .iter()
        .map(|(addr, e)| (addr.format(), format!("{e:#}")))
        .collect();
    // Results hold read locks on their cache entries; release them before the
    // next engine opens the same cache.
    drop(batch);
    crate::common::drain(rs).await;
    let mut outcomes = BTreeMap::new();
    let mut ok = BTreeSet::new();
    while let Ok(ev) = rx.try_recv() {
        match ev.kind {
            BuildEventKind::CacheOutcome { addr, outcome, .. } => {
                outcomes.insert(addr, outcome);
            }
            BuildEventKind::ResultEnd {
                addr, error: None, ..
            } => {
                ok.insert(addr);
            }
            _ => {}
        }
    }
    Ok(Run {
        failed,
        ok,
        outcomes,
    })
}

async fn run_addr(ws: &Ws, addr: &str) -> Run {
    run(ws, &Matcher::Addr(parse_addr(addr).expect("addr"))).await
}

fn everything() -> Matcher {
    Matcher::PackagePrefix(PkgBuf::from(""))
}

/// The failure log `addr`'s last failed run left in its slot, as
/// `heph inspect failure` finds it.
fn failure_log(ws: &Ws, addr: &str) -> String {
    let engine = ws.engine();
    let view = failure_view(engine.failure_slots(), &parse_addr(addr).expect("addr"))
        .expect("read the failure slot")
        .expect("a recorded failure");
    let log = view.log.expect("a failure log");
    std::fs::read_to_string(log).expect("read the failure log")
}

fn assert_passes(r: &Run, addr: &str) {
    assert!(
        r.ok.contains(addr) && !r.failed.contains_key(addr),
        "{addr} must pass: {r:#?}"
    );
}

fn assert_fails(r: &Run, addr: &str) {
    assert!(r.failed.contains_key(addr), "{addr} must fail: {r:#?}");
}

/// A helper named `*.BUILD` belongs to the package whose directory holds it
/// (I1): its type error is reported by that package's check, in the spec's
/// line format, and not by a package that loads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_reported_by_owner_only() {
    let ws = Ws::new(
        &[
            ("lib/BUILD", ""),
            (
                "lib/defs.BUILD",
                "def make(n: int) -> None:\n    bad: int = \"s\"\n",
            ),
            (
                "app/BUILD",
                "load(\"//lib/defs.BUILD\", \"make\")\n\ndef use() -> None:\n    make(1)\n",
            ),
        ],
        "",
    );
    let lib = run_addr(&ws, "//lib:build-lint-check").await;
    assert_fails(&lib, "//lib:build-lint-check");
    assert_eq!(
        failure_log(&ws, "//lib:build-lint-check").trim_end(),
        "lib/defs.BUILD:2:10: type-error: Expected type `int` but got `str`\n1 finding in //lib"
    );
    assert!(
        lib.failed["//lib:build-lint-check"].contains("1 finding in //lib"),
        "the run's error is the count line: {lib:#?}"
    );

    let app = run_addr(&ws, "//app:build-lint-check").await;
    assert_passes(&app, "//app:build-lint-check");
}

/// A loaded file in a directory with no BUILD file is owned by nobody (E1):
/// no package reports its findings, and it still types its callers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unowned_file_unreported() {
    let ws = Ws::new(
        &[
            (
                "tools/defs.star",
                "def make(n: int) -> None:\n    bad: int = \"s\"\n",
            ),
            (
                "app/BUILD",
                "load(\"//tools/defs.star\", \"make\")\n\ndef use() -> None:\n    make(1)\n",
            ),
            (
                "bad/BUILD",
                "load(\"//tools/defs.star\", \"make\")\n\ndef use() -> None:\n    make(\"s\")\n",
            ),
        ],
        "",
    );
    let r = run(&ws, &Matcher::Label("lint-check".to_string())).await;
    assert_passes(&r, "//app:build-lint-check");
    assert_fails(&r, "//bad:build-lint-check");
    let log = failure_log(&ws, "//bad:build-lint-check");
    assert!(
        log.starts_with("bad/BUILD:4:10: type-error:") && log.contains("1 finding in //bad"),
        "only the caller's own finding, typed through the unowned file: {log}"
    );
    assert!(!log.contains("tools/defs.star"), "{log}");
    assert!(
        !r.ok
            .iter()
            .chain(r.failed.keys())
            .any(|a| a.starts_with("//tools:")),
        "no package owns tools/: {r:#?}"
    );
}

/// A container display default types its parameter as a container of `Any`:
/// lint passes it without `requireTypeAnnotations`, and with it the finding
/// names the type and asks for one with no `Any`, under the same count line
/// and footer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_annotations_container_default() {
    let ws = Ws::new(
        &[(
            "pkg/BUILD",
            "def make_bin(srcs = [], deps = []):\n    pass\n",
        )],
        "",
    );
    assert_passes(
        &run_addr(&ws, "//pkg:build-lint-check").await,
        "//pkg:build-lint-check",
    );

    ws.set_options("requireTypeAnnotations: true");
    let r = run_addr(&ws, "//pkg:build-lint-check").await;
    assert_fails(&r, "//pkg:build-lint-check");
    assert_eq!(
        failure_log(&ws, "//pkg:build-lint-check").trim_end(),
        format!(
            "pkg/BUILD:1:14: annotation: parameter `srcs` of `make_bin` is typed `list` by its \
             default, a list of `Any`; add a type with no `Any`, e.g. `: list[str]`\n\
             pkg/BUILD:1:25: annotation: parameter `deps` of `make_bin` is typed `list` by its \
             default, a list of `Any`; add `: heph.types.Deps`\n\
             2 findings in //pkg\n\
             {FOOTER_ANNOTATIONS}"
        )
    );
}

/// `requireTypeAnnotations` gates an untyped `def` parameter: green without
/// it, a failure with the spec's text and footer with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_annotations_gate() {
    let ws = Ws::new(&[("pkg/BUILD", "def make_lib(x):\n    pass\n")], "");
    assert_passes(
        &run_addr(&ws, "//pkg:build-lint-check").await,
        "//pkg:build-lint-check",
    );

    ws.set_options("requireTypeAnnotations: true");
    let r = run_addr(&ws, "//pkg:build-lint-check").await;
    assert_fails(&r, "//pkg:build-lint-check");
    assert_eq!(
        failure_log(&ws, "//pkg:build-lint-check").trim_end(),
        format!(
            "pkg/BUILD:1:14: annotation: parameter `x` of `make_lib` has no type; add `: <type>`\n\
             1 finding in //pkg\n\
             {FOOTER_ANNOTATIONS}"
        )
    );
}

/// The option is in the lint def hash (I4): a pass cached with it off is not
/// reused once it is on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn option_flip_reruns() {
    let ws = Ws::new(&[("pkg/BUILD", "def make_lib(x):\n    pass\n")], "");
    let addr = "//pkg:build-lint-check";
    run_addr(&ws, addr).await;
    let warm = run_addr(&ws, addr).await;
    assert_eq!(
        warm.outcome(addr),
        Some(CacheOutcomeKind::LocalHit),
        "{warm:#?}"
    );

    ws.set_options("requireTypeAnnotations: true");
    let flipped = run_addr(&ws, addr).await;
    assert_eq!(
        flipped.outcome(addr),
        Some(CacheOutcomeKind::Miss),
        "{flipped:#?}"
    );
    assert_fails(&flipped, addr);
}

/// Every file a package loads, transitively, is an input of its lint check
/// (I4): an edit two loads away re-runs it, and the new verdict is the
/// edited file's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_edit_reruns() {
    let ws = Ws::new(
        &[
            (
                "pkg/BUILD",
                "load(\"//a/a.star\", \"f\")\n\ndef use() -> None:\n    f(1)\n",
            ),
            (
                "a/a.star",
                "load(\"//b/b.star\", \"g\")\n\ndef f(n: int) -> None:\n    g(n)\n",
            ),
            ("b/b.star", "def g(n: int) -> None:\n    pass\n"),
        ],
        "",
    );
    let addr = "//pkg:build-lint-check";
    assert_passes(&run_addr(&ws, addr).await, addr);
    let warm = run_addr(&ws, addr).await;
    assert_eq!(
        warm.outcome(addr),
        Some(CacheOutcomeKind::LocalHit),
        "{warm:#?}"
    );

    ws.write("b/b.star", "# edited\ndef g(n: int) -> None:\n    pass\n");
    let edited = run_addr(&ws, addr).await;
    assert_eq!(
        edited.outcome(addr),
        Some(CacheOutcomeKind::Miss),
        "{edited:#?}"
    );
    assert_passes(&edited, addr);
}

/// A `load("//dir")` reads every BUILD file of the directory, so a new one
/// there is a new input and re-runs the check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dir_load_new_file_reruns() {
    let ws = Ws::new(
        &[
            ("lib/BUILD", "X = 1\n"),
            (
                "pkg/BUILD",
                "load(\"//lib\", \"X\")\n\ndef use() -> int:\n    return X\n",
            ),
        ],
        "",
    );
    let addr = "//pkg:build-lint-check";
    assert_passes(&run_addr(&ws, addr).await, addr);
    let warm = run_addr(&ws, addr).await;
    assert_eq!(
        warm.outcome(addr),
        Some(CacheOutcomeKind::LocalHit),
        "{warm:#?}"
    );

    ws.write("lib/extra.BUILD", "Y = 2\n");
    let added = run_addr(&ws, addr).await;
    assert_eq!(
        added.outcome(addr),
        Some(CacheOutcomeKind::Miss),
        "{added:#?}"
    );
    assert_passes(&added, addr);
}

/// The def hash of `addr` on a fresh engine over `ws`.
async fn def_hash(ws: &Ws, addr: &str) -> Vec<u8> {
    let engine = ws.engine();
    let def = Arc::clone(&engine)
        .get_def(engine.new_state(), &parse_addr(addr).expect("addr"))
        .await
        .expect("resolve the def");
    def.target_def.hash.clone()
}

/// The option reaches the lint check's def and nothing else's (I3): an
/// ordinary target's def hash is the same with it on and off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn option_off_every_other_hash() {
    let ws = Ws::new(
        &[(
            "pkg/BUILD",
            "def make_lib(x):\n    pass\n\ntarget(name = \"t\", driver = \"textfile\", text = \"x\")\n",
        )],
        "",
    );
    let (t_off, lint_off) = (
        def_hash(&ws, "//pkg:t").await,
        def_hash(&ws, "//pkg:build-lint-check").await,
    );
    ws.set_options("requireTypeAnnotations: true");
    let (t_on, lint_on) = (
        def_hash(&ws, "//pkg:t").await,
        def_hash(&ws, "//pkg:build-lint-check").await,
    );
    assert_eq!(
        t_off, t_on,
        "an ordinary target's def must not see the option"
    );
    assert_ne!(lint_off, lint_on, "the lint check's def must");
}

/// A query target that opts out of provider exclusion (I3's one known
/// exception) now selects the check targets of BUILD-file packages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_target_sees_checks() {
    let ws = Ws::new(
        &[(
            "pkg/BUILD",
            "target(name = \"t\", driver = \"textfile\", text = \"x\", labels = [\"lint-check\"])\n",
        )],
        "",
    );
    let engine = ws.engine();
    let addr = parse_addr("//@heph/query:q@expr=label(lint-check),exclude_provider=__none__")
        .expect("addr");
    let spec = Arc::clone(&engine)
        .get_spec(engine.new_state(), &addr)
        .await
        .expect("resolve the query");
    let Some(Value::List(deps)) = spec.config.get("deps") else {
        panic!("deps is a list: {:?}", spec.config.get("deps"));
    };
    let deps: BTreeSet<String> = deps
        .iter()
        .map(|d| match d {
            Value::String(s) => parse_addr(s).expect("a dep addr").format(),
            other => panic!("a dep is an addr string: {other:?}"),
        })
        .collect();
    assert_eq!(
        deps,
        BTreeSet::from(["//pkg:build-lint-check".to_string(), "//pkg:t".to_string()]),
        "the lint-check label selects the user's target and the lint check only"
    );
}

/// Files every check passes on, already formatted.
const CLEAN: &[(&str, &str)] = &[
    (
        "a/BUILD",
        "load(\"//b/defs.BUILD\", \"make\")\n\ndef use() -> None:\n    make(1)\n",
    ),
    (
        "b/BUILD",
        "target(\n    name = \"t\",\n    driver = \"textfile\",\n    text = \"x\",\n)\n",
    ),
    ("b/defs.BUILD", "def make(n: int) -> None:\n    pass\n"),
];

const CHECKS: [&str; 3] = ["build-lint-check", "build-format-check", "build-format"];

/// A second `//...`: every check is a local hit, and `build-format` writes
/// nothing back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_run_all_hits() {
    let ws = Ws::new(CLEAN, "requireTypeAnnotations: true");
    let cold = run(&ws, &everything()).await;
    assert!(cold.failed.is_empty(), "{cold:#?}");
    let before: Vec<_> = CLEAN
        .iter()
        .map(|(p, _)| (ws.read(p), ws.stamp(p)))
        .collect();

    let warm = run(&ws, &everything()).await;
    assert!(warm.failed.is_empty(), "{warm:#?}");
    for pkg in ["a", "b"] {
        for check in CHECKS {
            let addr = format!("//{pkg}:{check}");
            assert_eq!(
                warm.outcome(&addr),
                Some(CacheOutcomeKind::LocalHit),
                "{addr}: {warm:#?}"
            );
        }
    }
    let after: Vec<_> = CLEAN
        .iter()
        .map(|(p, _)| (ws.read(p), ws.stamp(p)))
        .collect();
    assert_eq!(before, after, "a warm build-format writes nothing back");
}

/// `build-format` rewrites an unformatted file in place, the check passes on
/// what it wrote, and running it again changes nothing (I5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_writes_back_idempotent() {
    let unformatted = "target(name=\"t\",driver=\"textfile\",text=\"x\")\n";
    let ws = Ws::new(&[("pkg/BUILD", unformatted)], "");

    let check = run_addr(&ws, "//pkg:build-format-check").await;
    assert_fails(&check, "//pkg:build-format-check");
    let log = failure_log(&ws, "//pkg:build-format-check");
    assert!(
        log.starts_with("pkg/BUILD:1:1: format: not formatted"),
        "{log}"
    );
    assert!(
        log.trim_end()
            .ends_with("1 finding in //pkg\nrun heph run //pkg:build-format to fix"),
        "{log}"
    );
    assert_eq!(ws.read("pkg/BUILD"), unformatted, "a check writes nothing");

    assert_passes(
        &run_addr(&ws, "//pkg:build-format").await,
        "//pkg:build-format",
    );
    let formatted = ws.read("pkg/BUILD");
    assert_ne!(formatted, unformatted, "build-format wrote the file back");
    assert_passes(
        &run_addr(&ws, "//pkg:build-format-check").await,
        "//pkg:build-format-check",
    );

    assert_passes(
        &run_addr(&ws, "//pkg:build-format").await,
        "//pkg:build-format",
    );
    assert_eq!(ws.read("pkg/BUILD"), formatted, "formatting is idempotent");
}

/// A package with one unparseable file is not formatted at all: the run
/// fails naming that file, and neither file changes on disk, the formattable
/// one included. The package evaluates every `*.BUILD`, so here evaluation
/// refuses the file before `build-format` runs; the driver's own refusal of
/// an unparseable file, for one that changes between parse and run, is
/// `drivers::tests::format_unparseable_names_file`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_unparseable_leaves_package_untouched() {
    let unformatted = "target(name=\"t\",driver=\"textfile\",text=\"x\")\n";
    let unparseable = "x = (\n";
    let ws = Ws::new(
        &[("pkg/BUILD", unformatted), ("pkg/bad.BUILD", unparseable)],
        "",
    );
    let before = [ws.stamp("pkg/BUILD"), ws.stamp("pkg/bad.BUILD")];

    let m = Matcher::Addr(parse_addr("//pkg:build-format").expect("addr"));
    let err = match try_run(&ws, &m).await {
        Err(e) => format!("{e:#}"),
        Ok(r) => {
            assert_fails(&r, "//pkg:build-format");
            r.failed["//pkg:build-format"].clone()
        }
    };
    assert!(err.contains("pkg/bad.BUILD"), "names the file: {err}");

    assert_eq!(ws.read("pkg/BUILD"), unformatted);
    assert_eq!(ws.read("pkg/bad.BUILD"), unparseable);
    assert_eq!(
        [ws.stamp("pkg/BUILD"), ws.stamp("pkg/bad.BUILD")],
        before,
        "nothing was written back"
    );
}

/// Through the real sandbox and in-place write-back, `build-format` keeps
/// each rewritten file's mode on disk: an executable stays executable, and
/// neither becomes read-only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_keeps_mode_on_disk() {
    let ws = Ws::new(
        &[
            ("pkg/BUILD", "x    =  1\n"),
            ("pkg/more.BUILD", "y    =  2\n"),
        ],
        "",
    );
    ws.set_mode("pkg/BUILD", 0o755);
    ws.set_mode("pkg/more.BUILD", 0o644);

    assert_passes(
        &run_addr(&ws, "//pkg:build-format").await,
        "//pkg:build-format",
    );
    assert_eq!(ws.read("pkg/BUILD"), "x = 1\n");
    assert_eq!(ws.read("pkg/more.BUILD"), "y = 2\n");
    assert_eq!(ws.mode("pkg/BUILD"), 0o755, "{:o}", ws.mode("pkg/BUILD"));
    assert_eq!(
        ws.mode("pkg/more.BUILD"),
        0o644,
        "{:o}",
        ws.mode("pkg/more.BUILD")
    );
}

/// `indent` is in the format def hash (I6): a pass cached at the default
/// indent is not reused at another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indent_change_reruns() {
    let ws = Ws::new(&[("pkg/BUILD", "def f() -> None:\n    pass\n")], "");
    let addr = "//pkg:build-format-check";
    assert_passes(&run_addr(&ws, addr).await, addr);

    ws.set_options("indent: 2");
    let r = run_addr(&ws, addr).await;
    assert_eq!(r.outcome(addr), Some(CacheOutcomeKind::Miss), "{r:#?}");
    assert_fails(&r, addr);
}

/// The documented CI selector, parsed as `heph run -e` parses it, selects
/// the lint and format checks of every package and nothing else: not
/// `build-format`, which writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_selector() {
    let ws = Ws::new(
        &[
            ("lint/BUILD", "def f() -> None:\n    x: int = \"s\"\n"),
            ("fmt/BUILD", "x=1\n"),
        ],
        "",
    );
    let m = heph::htquery::parse(CI_SELECTOR, &PkgBuf::from("")).expect("the selector parses");

    let engine = ws.engine();
    let selected: BTreeSet<String> = Arc::clone(&engine)
        .query(engine.new_state(), &m, Discovery::Complete)
        .map_ok(|a| a.format())
        .try_collect()
        .await
        .expect("query");
    drop(engine);
    let want: BTreeSet<String> = ["lint", "fmt"]
        .iter()
        .flat_map(|p| ["build-lint-check", "build-format-check"].map(|c| format!("//{p}:{c}")))
        .collect();
    assert_eq!(selected, want);

    let r = run(&ws, &m).await;
    assert_eq!(
        r.failed.keys().map(String::as_str).collect::<Vec<_>>(),
        ["//fmt:build-format-check", "//lint:build-lint-check"],
        "{r:#?}"
    );
    assert_eq!(ws.read("fmt/BUILD"), "x=1\n", "nothing was formatted");
}

/// A user target named like a check shadows it (I2): `//...` stays green and
/// runs the user's target, and the record `heph validate` fails on names the
/// conflict. (The validate failure itself is pinned by
/// `validate::tests::shadowed_check_target_fails` in `src/commands/validate.rs`,
/// over the same record; `ValidateApp` is not reachable from here.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_runs_user_target_validate_fails() {
    let ws = Ws::new(
        &[(
            "pkg/BUILD",
            "target(\n    name = \"build-lint-check\",\n    driver = \"textfile\",\n    text = \"mine\",\n)\n",
        )],
        "",
    );
    let r = run(&ws, &everything()).await;
    assert!(r.failed.is_empty(), "{r:#?}");
    for check in CHECKS {
        assert!(r.ok.contains(&format!("//pkg:{check}")), "{check}: {r:#?}");
    }

    let engine = ws.engine();
    let result = Arc::clone(&engine)
        .result_addr(
            engine.new_state(),
            &parse_addr("//pkg:build-lint-check").expect("addr"),
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .expect("the user's target");
    assert_eq!(crate::common::artifact_string(&result), "mine");
    drop(result);

    let conflicts: Vec<String> = ws.shadowed.all().iter().map(ToString::to_string).collect();
    assert_eq!(
        conflicts,
        [
            "//pkg:build-lint-check, declared at pkg/BUILD:1, shadows the BUILD-file check \
          `build-lint-check`: rename the target, or remove it to get the check back"
        ]
    );
}

/// `heph.types.bash.Kwargs` is built from the schema of the `bash` driver the
/// shipped engine registers: a wrong-typed key is a lint finding in the
/// spec's line format, the fixed call passes and builds through the real
/// driver, and an unknown key fails evaluation naming the key (D8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_kwargs_real_driver() {
    const LIB: &str = "\
def lib(name: str, **kwargs: heph.types.bash.Kwargs):
    target(name = name, driver = \"bash\", **kwargs)
";
    let ws = Ws::new(
        &[(
            "pkg/BUILD",
            &format!("{LIB}\ndef app():\n    lib(name = \"a\", run = 1)\n"),
        )],
        "",
    );
    // `bash` is a factory: its driver exists only when `plugins:` names it.
    ws.write(
        ".hephconfig",
        "plugins:\n  - builtin: buildfile\n  - builtin: bash\n",
    );
    let r = run_addr(&ws, "//pkg:build-lint-check").await;
    assert_fails(&r, "//pkg:build-lint-check");
    assert_eq!(
        failure_log(&ws, "//pkg:build-lint-check").trim_end(),
        "pkg/BUILD:5:21: type-error: Expected type `None | str | list[str]` but got `int`\n\
         1 finding in //pkg"
    );

    ws.write(
        "pkg/BUILD",
        &format!(
            "{LIB}\ndef app():\n    lib(name = \"a\", run = \"true\")\n\n\
             lib(name = \"t\", run = \"true\")\n"
        ),
    );
    // Only the two targets under test: this BUILD is not formatted, so
    // `everything()` would run `build-format`, whose write-back races the
    // lint check's copy of the same file ("modified after it was hashed").
    let m = Matcher::Or(
        ["//pkg:build-lint-check", "//pkg:t"]
            .into_iter()
            .map(|a| Matcher::Addr(parse_addr(a).expect("addr")))
            .collect(),
    );
    let r = run(&ws, &m).await;
    assert_passes(&r, "//pkg:build-lint-check");
    assert_passes(&r, "//pkg:t");

    ws.write(
        "pkg/BUILD",
        &format!("{LIB}\nlib(name = \"t\", rnu = \"true\")\n"),
    );
    let m = Matcher::Addr(parse_addr("//pkg:build-lint-check").expect("addr"));
    let err = match try_run(&ws, &m).await {
        Err(e) => format!("{e:#}"),
        Ok(r) => {
            assert_fails(&r, "//pkg:build-lint-check");
            r.failed["//pkg:build-lint-check"].clone()
        }
    };
    assert!(
        err.contains("Unexpected key `rnu` for argument `kwargs`"),
        "names the key: {err}"
    );
}

/// The managed bash driver registered as `kw`, its schema with one more
/// field when `extra`: two engines whose driver schemas differ by a field.
struct KwBash {
    inner: heph::pluginexec::Driver,
    extra: bool,
}

#[async_trait::async_trait]
impl heph::engine::driver_managed::ManagedDriver for KwBash {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "kw".to_string(),
        })
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        let mut schema = heph::engine::driver_managed::ManagedDriver::schema(&self.inner);
        if self.extra {
            schema.fields.push(hplugin::driver::DriverField {
                name: "extra".to_string(),
                ty: hcore::htvalue::signature::ParamType::String,
                doc: "a field only one engine's driver has".to_string(),
                required: false,
            });
        }
        schema
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        heph::engine::driver_managed::ManagedDriver::parse(&self.inner, req, ctoken).await
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
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

/// I2: the drivers' `Kwargs` reach no ordinary target. One wrapper, typed
/// `**kwargs: heph.types.kw.Kwargs` or bare, under driver schemas that differ
/// by a field: the target's spec and def hash are the same all four ways.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_kwargs_reach_no_target_def() {
    let ws = Ws::new(&[], "");
    let addr = parse_addr("//pkg:t").expect("addr");
    let mut seen = Vec::new();
    for annotation in [": heph.types.kw.Kwargs", ""] {
        ws.write(
            "pkg/BUILD",
            &format!(
                "def lib(name: str, **kwargs{annotation}):\n    \
                 target(name = name, driver = \"kw\", **kwargs)\n\n\
                 lib(name = \"t\", run = \"echo hi > $OUT\", out = \"o.txt\")\n"
            ),
        );
        for extra in [false, true] {
            let engine = ws.engine_with(|e| {
                e.register_managed_driver(move |_| {
                    Box::new(KwBash {
                        inner: heph::pluginexec::Driver::new_bash().with_host_path(),
                        extra,
                    })
                })
                .expect("register kw");
            });
            assert_eq!(
                engine
                    .driver_schema("kw")
                    .expect("kw")
                    .fields
                    .iter()
                    .any(|f| f.name == "extra"),
                extra
            );
            let spec = Arc::clone(&engine)
                .get_spec(engine.new_state(), &addr)
                .await
                .unwrap_or_else(|e| panic!("{annotation:?}, extra {extra}: {e:#}"));
            let def = Arc::clone(&engine)
                .get_def(engine.new_state(), &addr)
                .await
                .unwrap_or_else(|e| panic!("{annotation:?}, extra {extra}: {e:#}"));
            seen.push((
                format!("{annotation:?}, extra {extra}"),
                // A JSON value: its maps are sorted, the spec's `config` is not.
                serde_json::to_value(&*spec.spec).expect("spec to json"),
                def.target_def.hash.clone(),
            ));
        }
    }
    let (first, rest) = seen.split_first().expect("four evaluations");
    for (label, spec, hash) in rest {
        assert_eq!(spec, &first.1, "{label}: spec, against {}", first.0);
        assert_eq!(hash, &first.2, "{label}: def hash, against {}", first.0);
    }
}
