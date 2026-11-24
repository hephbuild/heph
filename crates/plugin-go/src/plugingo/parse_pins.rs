//! What each Go driver's `parse` hands the engine, pinned.
//!
//! The def hash reaches every cache key, the inputs decide what is staged and
//! hashed, and the raw def is what `run` reads. A refactor of `parse` must leave
//! all of them byte-identical, so each driver parses one spec exercising every
//! dep-group shape it treats specially, and the result is compared with a
//! golden:
//!
//! - `hash` is the def hash itself, readable.
//! - `inputs`, `outputs` and `raw_def` are an xxh3 of their JSON serialization.
//!
//! A moved `hash` invalidates every cached target of that driver: update it only
//! alongside a deliberate format-version bump.

use super::driver_compile::GoCompileDriver;
use super::driver_download::GoDownloadDriver;
use super::driver_format::GoFormatDriver;
use super::driver_golist::GoGolistDriver;
use super::driver_lint::{GoLintDriver, GoLintFixDriver, GoLintGateDriver};
use super::driver_testmain::GoTestmainDriver;
use super::toolchain::GoToolchainDriver;
use hcore::htvalue::Value;
use hdriver_support::driver_managed::ManagedDriver;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::ParseRequest;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;
use std::sync::Arc;

const RUNNER: &str = "//tools/devenv:runner";

#[derive(Debug, PartialEq, Eq)]
struct Pin {
    hash: String,
    inputs: u64,
    outputs: u64,
    raw_def: u64,
}

fn golden(hash: &str, inputs: u64, outputs: u64, raw_def: u64) -> Pin {
    Pin {
        hash: hash.to_string(),
        inputs,
        outputs,
        raw_def,
    }
}

fn xxh3_json(v: &(impl serde::Serialize + ?Sized)) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&serde_json::to_vec(v).expect("serialize"))
}

async fn pin(driver: &impl ManagedDriver, spec: TargetSpec) -> Pin {
    let ct = hcore::hasync::StdCancellationToken::new();
    let def = driver
        .parse(
            ParseRequest {
                request_id: "t".to_string(),
                target_spec: Arc::new(spec),
            },
            &ct,
        )
        .await
        .expect("parse")
        .target_def;
    Pin {
        hash: String::from_utf8(def.hash.clone()).expect("hash is hex"),
        inputs: xxh3_json(&def.inputs),
        outputs: xxh3_json(&def.outputs),
        raw_def: xxh3_json(&*def.raw_def),
    }
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn list(vs: &[&str]) -> Value {
    Value::List(vs.iter().map(|v| s(v)).collect())
}

fn groups(gs: &[(&str, &[&str])]) -> Value {
    Value::Map(gs.iter().map(|(g, vs)| (g.to_string(), list(vs))).collect())
}

/// One output group only: `out` is a `HashMap`, so two groups would come out in
/// iteration order and the pin would flake.
fn spec(driver: &str, config: &[(&str, Value)]) -> TargetSpec {
    TargetSpec {
        addr: Addr::new(PkgBuf::from("app/lib"), "t".to_string(), Default::default()),
        driver: driver.to_string(),
        config: config
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<HashMap<_, _>>(),
        ..Default::default()
    }
}

#[tokio::test]
async fn compile() {
    let spec = spec(
        "go_compile",
        &[
            ("p_flag", s("example.com/app/lib")),
            ("out_file", s("lib.a")),
            ("goos", s("linux")),
            ("goarch", s("amd64")),
            ("go_version", s("1.26.4")),
            ("go_module", s("example.com/app")),
            ("go_variant", s("v")),
            ("import_paths", list(&["fmt", "example.com/app/dep"])),
            ("s_files", list(&["a.s"])),
            ("embed_variant", list(&["embed"])),
            (
                "deps",
                groups(&[
                    ("", &["//app/lib:a.go", "//app/lib:b.go"]),
                    ("gosdk", &["//@heph/go/sdk:go"]),
                    ("embed_src", &["//app/lib:assets", "//app/lib:more"]),
                    ("golist", &["//app/lib:_golist"]),
                    ("lib", &["//app/dep:build_lib"]),
                ]),
            ),
            ("out", groups(&[("a", &["lib.a"])])),
        ],
    );
    let driver = GoCompileDriver::new().with_default_runner(Some(RUNNER.to_string()));
    assert_eq!(
        pin(&driver, spec).await,
        golden(
            "9bc2ca945bf9d0c0",
            1171191965469363763,
            10521890657529364954,
            7455536168786717017
        )
    );
}

#[tokio::test]
async fn golist() {
    let spec = spec(
        "go_golist",
        &[
            ("import_path", s("example.com/app/lib")),
            ("goos", s("linux")),
            ("goarch", s("amd64")),
            ("go_version", s("1.26.4")),
            ("go_module", s("example.com/app")),
            ("go_variant", s("v")),
            ("build_tags", list(&["tag"])),
            ("with_test", Value::Bool(true)),
            ("firstparty", Value::Bool(true)),
            (
                "deps",
                groups(&[
                    ("srcfiles", &["//app/lib:a.go", "//app/lib:b.go"]),
                    ("modfiles", &["//app:go.mod"]),
                    ("gosdk", &["//@heph/go/sdk:go"]),
                ]),
            ),
            ("out", groups(&[("", &["golist.json"])])),
        ],
    );
    let driver = GoGolistDriver::new().with_default_runner(Some(RUNNER.to_string()));
    // `raw_def` moved when the def stopped carrying a copy of its dep inputs
    // (they are hashed, never read by `run`); `hash` and `inputs` did not.
    assert_eq!(
        pin(&driver, spec).await,
        golden(
            "cbd4cbd43b71a9f0",
            18258194127413756051,
            14850982951679174278,
            6272041238485915835
        )
    );
}

#[tokio::test]
async fn lint() {
    let spec = spec(
        "go_lint",
        &[
            ("import_path", s("example.com/app/lib")),
            ("goos", s("linux")),
            ("goarch", s("amd64")),
            ("go_version", s("1.26.4")),
            ("import_paths", list(&["fmt"])),
            (
                "deps",
                groups(&[
                    ("", &["//app/lib:a.go"]),
                    ("govet_tool", &["//@heph/go/govet:govet"]),
                    ("lib", &["//app/dep:build_lib"]),
                ]),
            ),
            ("out", groups(&[("", &["lint-report.json"])])),
        ],
    );
    let driver = GoLintDriver::new().with_default_runner(Some(RUNNER.to_string()));
    assert_eq!(
        pin(&driver, spec).await,
        golden(
            "896b67281d8a29fa",
            17711523505509082636,
            1760905735178133697,
            6533136794774042437
        )
    );
}

fn report_deps() -> Value {
    groups(&[
        ("report", &["//app/lib:analyze_a", "//app/lib:analyze_b"]),
        ("config", &["//:.golangci.yml"]),
        ("", &["//app/lib:a.go"]),
    ])
}

#[tokio::test]
async fn lint_gate() {
    let spec = spec("go_lint_gate", &[("deps", report_deps())]);
    assert_eq!(
        pin(&GoLintGateDriver::new(), spec).await,
        golden(
            "db02334e96d65708",
            561984990895316533,
            9444422325801888874,
            16967360502811430677
        )
    );
}

#[tokio::test]
async fn lint_fix() {
    let spec = spec(
        "go_lint_fix",
        &[("deps", report_deps()), ("out", groups(&[("", &["a.go"])]))],
    );
    assert_eq!(
        pin(&GoLintFixDriver::new(), spec).await,
        golden(
            "db02334e96d65708",
            561984990895316533,
            7464485803555944410,
            16967360502811430677
        )
    );
}

fn format_spec(driver: &str) -> TargetSpec {
    spec(
        driver,
        &[(
            "deps",
            groups(&[
                ("", &["//app/lib:a.go", "//app/lib:b.go"]),
                ("govet_tool", &["//@heph/go/govet:govet"]),
                ("config", &["//:.golangci.yml"]),
            ]),
        )],
    )
}

#[tokio::test]
async fn format() {
    let driver = GoFormatDriver::fix().with_default_runner(Some(RUNNER.to_string()));
    assert_eq!(
        pin(&driver, format_spec("go_format")).await,
        golden(
            "d1dd927ed529f768",
            7132676117301867560,
            9887955642903338788,
            3122745946226892598
        )
    );
}

#[tokio::test]
async fn format_check() {
    let driver = GoFormatDriver::check().with_default_runner(Some(RUNNER.to_string()));
    assert_eq!(
        pin(&driver, format_spec("go_format_check")).await,
        golden(
            "8cd045b8efcef9bd",
            7132676117301867560,
            9444422325801888874,
            17547657357643910355
        )
    );
}

#[tokio::test]
async fn download() {
    let spec = spec(
        "go.download",
        &[
            ("module", s("gopkg.in/yaml.v2")),
            ("version", s("v2.4.0")),
            ("go_version", s("1.26.4")),
            ("deps", groups(&[("gosdk", &["//@heph/go/sdk:go"])])),
        ],
    );
    let driver = GoDownloadDriver::new().with_default_runner(Some(RUNNER.to_string()));
    assert_eq!(
        pin(&driver, spec).await,
        golden(
            "b50bc47169b7d5a",
            9146695676973023819,
            15567405449820001154,
            598820809440565308
        )
    );
}

#[tokio::test]
async fn testmain() {
    let spec = spec(
        "go_testmain",
        &[
            (
                "deps",
                groups(&[
                    ("golist", &["//app/lib:_golist", "//app/lib:_golist_x"]),
                    ("test", &["//app/lib:a_test.go"]),
                    ("xtest", &["//app/lib:b_test.go", "//app/lib:c_test.go"]),
                    ("ignored", &["//app/lib:not_an_input"]),
                ]),
            ),
            ("out", groups(&[("", &["testmain.go"])])),
        ],
    );
    assert_eq!(
        pin(&GoTestmainDriver, spec).await,
        golden(
            "cf25fdf5595853ba",
            13364168438215395042,
            15317576171529450432,
            4012289759480614559
        )
    );
}

#[tokio::test]
async fn toolchain() {
    let spec = spec(
        "go_toolchain",
        &[
            ("version", s("1.26.4")),
            ("goos", s("linux")),
            ("goarch", s("amd64")),
            ("sha256", s("abc")),
            ("out", groups(&[("", &["go/"])])),
        ],
    );
    assert_eq!(
        pin(&GoToolchainDriver, spec).await,
        golden(
            "1f9b01b2ec3ec8ae",
            9444422325801888874,
            11215949886407070152,
            4146145119391290628
        )
    );
}
