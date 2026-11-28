//! `heph tool codegen` at the process boundary: its exit codes (0, 1 for
//! findings and for a failure, clap's 2 for a usage error; U15) reach the
//! shell as statuses, and commands that build no engine leave no codegen
//! index behind (Lifecycle row 22).
//!
//! Neither has an in-process form: a linked test sees a `CodegenExit` value,
//! never the status `main` turns it into, nor clap's own usage-error exit.

mod common;

use std::path::Path;

use common::{Dist, Workspace, describe};

/// A `copy` target writing `gen.txt` in package `p`.
const COPY_TARGET: &str = r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf v1 > $OUT",
    out = "gen.txt",
    codegen = "copy",
)
"#;

fn code(ws: &Workspace, dist: &Dist, args: &[&str]) -> (Option<i32>, String) {
    let out = ws.run(dist, args).expect("run heph");
    (out.status.code(), describe(&out))
}

const CHECK: [&str; 4] = ["tool", "codegen", "gitignore", "--check"];

/// Stale is a finding: exit 1 with the report on stdout; once written,
/// clean is 0.
#[test]
fn tool_codegen_findings_exit_1() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write("p/BUILD", COPY_TARGET).expect("BUILD");

    // No section yet: stale is a finding.
    let out = ws.run(&dist, &CHECK).expect("run heph");
    assert_eq!(out.status.code(), Some(1), "stale: {}", describe(&out));
    assert!(!out.stdout.is_empty(), "findings print: {}", describe(&out));
    assert!(
        !ws.root().join(".gitignore").exists(),
        "--check writes nothing"
    );

    let (c, out) = code(&ws, &dist, &["tool", "codegen", "gitignore"]);
    assert_eq!(c, Some(0), "write: {out}");
    let (c, out) = code(&ws, &dist, &CHECK);
    assert_eq!(c, Some(0), "clean: {out}");
}

/// A failure exits 1 like findings (U15), with nothing on stdout; clap's
/// usage errors keep 2.
#[test]
fn tool_codegen_failure_exit_1() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write("p/BUILD", COPY_TARGET).expect("BUILD");

    // A BUILD file that fails to evaluate is an error, not a finding.
    ws.write("q/BUILD", "target(name = \n")
        .expect("broken BUILD");
    let out = ws.run(&dist, &CHECK).expect("run heph");
    assert_eq!(out.status.code(), Some(1), "error: {}", describe(&out));
    assert!(
        out.stdout.is_empty(),
        "an error prints no report: {}",
        describe(&out)
    );

    // A clap usage error exits 2.
    let (c, out) = code(
        &ws,
        &dist,
        &["tool", "codegen", "gitignore", "--no-such-flag"],
    );
    assert_eq!(c, Some(2), "usage error: {out}");
    let (c, out) = code(&ws, &dist, &["tool", "codegen", "why"]);
    assert_eq!(c, Some(2), "missing argument: {out}");
}

/// Every path under `dir` (recursively), for an assertion message.
fn tree(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        out.push(p.display().to_string());
        if p.is_dir() && !p.is_symlink() {
            tree(&p, out);
        }
    }
}

#[test]
fn help_version_and_engineless_commands_create_nothing() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write("p/BUILD", COPY_TARGET).expect("BUILD");
    ws.write("p/gen.txt", "v1").expect("generated file");
    ws.write(
        ".gitignore",
        "# BEGIN heph-generated (managed by `heph tool codegen gitignore` — do not edit)\n# //p:gen\n/p/gen.txt\n# END heph-generated\n",
    )
    .expect(".gitignore");

    for args in [
        &["--help"][..],
        &["version"][..],
        &["tool", "codegen", "--help"][..],
        &["tool", "codegen", "gitignore", "--help"][..],
        &["tool", "codegen", "status", "--help"][..],
        &["tool", "gen-gitignore"][..],
    ] {
        let out = ws.run(&dist, args).expect("run heph");
        let want = if args == ["tool", "gen-gitignore"] {
            Some(1)
        } else {
            Some(0)
        };
        assert_eq!(out.status.code(), want, "{args:?}: {}", describe(&out));
    }

    let mut paths = Vec::new();
    tree(ws.root(), &mut paths);
    tree(ws.home(), &mut paths);
    let created: Vec<&String> = paths
        .iter()
        .filter(|p| {
            Path::new(p)
                .components()
                .any(|c| c.as_os_str() == "codegen")
                || p.ends_with(".db")
        })
        .collect();
    assert!(
        created.is_empty(),
        "no index db and no codegen dir: {created:?}\nall: {paths:#?}"
    );
}
