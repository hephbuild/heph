//! `heph tool codegen` at the process boundary: its 0 / 1 / 2 exit codes
//! (Decision 20) reach the shell as statuses, and commands that build no
//! engine leave no codegen index behind (Lifecycle row 22).
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

#[test]
fn codegen_check_exit_codes() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write("p/BUILD", COPY_TARGET).expect("BUILD");
    let check = ["tool", "codegen", "gitignore", "--check"];

    // No section yet: stale is a finding.
    let (c, out) = code(&ws, &dist, &check);
    assert_eq!(c, Some(1), "stale: {out}");
    assert!(
        !ws.root().join(".gitignore").exists(),
        "--check writes nothing"
    );

    let (c, out) = code(&ws, &dist, &["tool", "codegen", "gitignore"]);
    assert_eq!(c, Some(0), "write: {out}");
    let (c, out) = code(&ws, &dist, &check);
    assert_eq!(c, Some(0), "clean: {out}");

    // A BUILD file that fails to evaluate is an error, not a finding.
    ws.write("q/BUILD", "target(name = \n")
        .expect("broken BUILD");
    let (c, out) = code(&ws, &dist, &check);
    assert_eq!(c, Some(2), "error: {out}");

    // A clap usage error exits 2 as well.
    let (c, out) = code(
        &ws,
        &dist,
        &["tool", "codegen", "gitignore", "--no-such-flag"],
    );
    assert_eq!(c, Some(2), "usage error: {out}");
    let (c, out) = code(&ws, &dist, &["tool", "codegen", "why"]);
    assert_eq!(c, Some(2), "missing argument: {out}");

    // Every other heph command still exits 1 on an error.
    let (c, out) = code(&ws, &dist, &["run", "//p:missing"]);
    assert_eq!(c, Some(1), "a non-codegen error: {out}");
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
            Some(2)
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
