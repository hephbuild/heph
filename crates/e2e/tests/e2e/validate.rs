//! `heph validate` over a whole workspace, through the engine the command
//! builds (error-reporting D8, CLI-17 / STAGE-21).

use heph::commands::validate::{ValidateView, validate_workspace_at};

const CONFIG: &str = "plugins:\n  - builtin: buildfile\n  - builtin: exec\n  - builtin: bash\ntelemetry:\n  enabled: false\n";

/// Validate the workspace made of `files`.
async fn validate(files: &[(&str, &str)]) -> ValidateView {
    let dir = hcore::fsutil::tempdir().expect("tempdir");
    for (path, content) in files {
        let abs = dir.path().join(path);
        std::fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
        std::fs::write(&abs, content).expect("write");
    }
    validate_workspace_at(dir.path()).await.expect("validate")
}

/// How many of `lines` say `needle`.
fn count(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|l| l.contains(needle)).count()
}

/// CLI-17 / STAGE-21: a cause reached from several targets, and by several of
/// validate's checks, was listed once per path: four problems for one broken
/// BUILD file, and a `codegen:` warning for each target it broke.
#[tokio::test]
async fn validate_reports_each_cause_once() {
    let view = validate(&[
        (".hephconfig", CONFIG),
        (
            "a/BUILD",
            r#"
target(name = "top", driver = "bash", run = "true", deps = ["//b:missing"])
target(name = "other", driver = "bash", run = "true", deps = ["//b:missing"])
"#,
        ),
        ("b/BUILD", "x = undefined_one\n"),
        ("c/BUILD", r#"target(name = "t", driver = "bash", run = 1)"#),
        ("d/BUILD", r#"target(name = "u", driver = "nosuchdriver")"#),
    ])
    .await;
    let ValidateView { problems, warnings } = &view;
    for cause in [
        "Variable `undefined_one` not found",
        "expected string or [string], got: 1",
        "nosuchdriver",
    ] {
        assert_eq!(count(problems, cause), 1, "{cause} once in {problems:#?}");
        assert_eq!(
            count(warnings, cause),
            0,
            "{cause} not warned: {warnings:#?}"
        );
    }
    assert_eq!(problems.len(), 3, "one problem per cause: {problems:#?}");
}

/// CLI-17: a fresh workspace with no codegen target and no `.gitignore`
/// failed validate until `heph tool codegen gitignore` wrote an empty section.
#[tokio::test]
async fn validate_skips_gitignore_check_without_codegen() {
    let view = validate(&[
        (".hephconfig", CONFIG),
        (
            "p/BUILD",
            r#"target(name = "t", driver = "bash", run = "true")"#,
        ),
    ])
    .await;
    assert_eq!(view.problems, Vec::<String>::new());
    assert_eq!(view.warnings, Vec::<String>::new());

    // A section left behind by codegen targets that are gone is still stale.
    let view = validate(&[
        (".hephconfig", CONFIG),
        (
            "p/BUILD",
            r#"target(name = "t", driver = "bash", run = "true")"#,
        ),
        (
            ".gitignore",
            &format!(
                "{}\n/p/gen.rs\n{}\n",
                heph::htwalk::gitignore_section::BEGIN_MARKER,
                heph::htwalk::gitignore_section::END_MARKER
            ),
        ),
    ])
    .await;
    assert_eq!(
        count(&view.problems, ".gitignore"),
        1,
        "{:#?}",
        view.problems
    );
}
