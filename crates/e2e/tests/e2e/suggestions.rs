//! "Did you mean" (error-reporting D4, F4, I4): an error naming an unknown
//! thing suggests the nearest known one, as `DidYouMean` hints on the
//! recorded failure's diagnostic. The near-miss helper itself is unit-tested
//! in `hcore::suggest`; these pin what each error site feeds it and what the
//! record carries.

use crate::source_location::{at, fail, loc, text, triple, ws_with};
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
