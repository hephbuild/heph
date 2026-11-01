#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `GOPRIVATE` modules fetched directly with `git`, under the hermetic SDK.
//!
//! Nothing here goes near a proxy: `url.insteadOf` in the user's `~/.gitconfig`
//! points each module's URL at a local repository, so the test proves the whole
//! path a private module takes (the host's `git`, its global config, the
//! `GOPRIVATE` bypass, and then listing and compiling what was fetched) without a
//! network or a credential. Module paths carry a `.git` qualifier because that is
//! what makes `go` skip its HTTPS `go-get` discovery.
//!
//! A file of its own because it sets `HOME` and `GOPRIVATE` for the process: this
//! test binary holds this one test, so nothing races those writes.

mod common;

use common::{HERMETIC_GO, artifact_paths, make_workspace_hermetic, require_go};
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
        .args(args)
        .current_dir(dir)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// A repository holding one module tagged `v1.0.0`, with one file per
/// `(name, content)`.
fn tagged_repo(dir: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, content) in files {
        std::fs::write(dir.join(name), content)?;
    }
    git(dir, &["init", "-q", "-b", "main", "."])?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-qm", "init"])?;
    git(dir, &["tag", "v1.0.0"])
}

/// A project importing `example.test/acme/<dep>.git`, with the go.mod and go.sum a
/// real project has checked in.
fn app_importing(dep: &str, tidy_modcache: &Path) -> anyhow::Result<tempfile::TempDir> {
    let app = tempfile::tempdir()?;
    std::fs::write(
        app.path().join("go.mod"),
        format!(
            "module example.test/app\n\ngo 1.21\n\n\
             require example.test/acme/{dep}.git v1.0.0\n"
        ),
    )?;
    std::fs::write(
        app.path().join("app.go"),
        format!("package app\n\nimport \"example.test/acme/{dep}.git\"\n\nconst V = {dep}.V\n"),
    )?;
    let tidy = Command::new("go")
        .args(["mod", "tidy"])
        .env("GOFLAGS", "-modcacherw")
        .env("GOMODCACHE", tidy_modcache)
        .env("GOTOOLCHAIN", "local")
        .env("GOWORK", "off")
        .current_dir(app.path())
        .output()?;
    anyhow::ensure!(
        tidy.status.success(),
        "go mod tidy: {}",
        String::from_utf8_lossy(&tidy.stderr)
    );
    Ok(app)
}

#[tokio::test]
async fn private_modules_fetched_with_git_under_the_hermetic_sdk() -> anyhow::Result<()> {
    require_go!();
    if which::which("git").is_err() {
        eprintln!("skipping private module e2e: no git on PATH");
        return Ok(());
    }

    let scratch = tempfile::tempdir()?;
    let (repos, home) = (scratch.path().join("repos"), scratch.path().join("home"));
    let tidy_modcache = scratch.path().join("tidy-modcache");
    std::fs::create_dir_all(&home)?;

    // `a` imports `b`, so listing and compiling `a` has to find `b` too.
    tagged_repo(
        &repos.join("b"),
        &[
            ("go.mod", "module example.test/acme/b.git\n\ngo 1.21\n"),
            ("b.go", "package b\n\nconst V = \"b\"\n"),
        ],
    )?;
    tagged_repo(
        &repos.join("a"),
        &[
            (
                "go.mod",
                "module example.test/acme/a.git\n\ngo 1.21\n\n\
                 require example.test/acme/b.git v1.0.0\n",
            ),
            (
                "a.go",
                "package a\n\nimport \"example.test/acme/b.git\"\n\nconst V = \"a\" + b.V\n",
            ),
        ],
    )?;
    // Exists for `go mod tidy`, then goes away: what an expired credential looks like
    // from `go`'s side, without needing a real one.
    tagged_repo(
        &repos.join("gone"),
        &[
            ("go.mod", "module example.test/acme/gone.git\n\ngo 1.21\n"),
            ("gone.go", "package gone\n\nconst V = \"gone\"\n"),
        ],
    )?;

    let mut gitconfig = String::new();
    for name in ["a", "b", "gone"] {
        gitconfig.push_str(&format!(
            "[url \"file://{}\"]\n\tinsteadOf = https://example.test/acme/{name}\n",
            repos.join(name).display()
        ));
    }
    std::fs::write(home.join(".gitconfig"), gitconfig)?;

    // SAFETY: the only test in this binary, and no thread reads the environment yet.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("GOPRIVATE", "example.test");
        for inherited in [
            "GOFLAGS",
            "GOMODCACHE",
            "GOPATH",
            "GONOPROXY",
            "GONOSUMDB",
            "GOPROXY",
            "GOSUMDB",
            "GIT_CONFIG_GLOBAL",
            "XDG_CONFIG_HOME",
        ] {
            std::env::remove_var(inherited);
        }
    }

    let ws = make_workspace_hermetic(app_importing("a", &tidy_modcache)?)?;
    let sdk = format!("//@heph/go/toolchain/{HERMETIC_GO}:go");
    if let Err(e) = ws.run(&sdk).await {
        eprintln!("skipping private module e2e (SDK unavailable): {e:?}");
        return Ok(());
    }
    let result = ws.run("//:build_lib@v=host").await?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "a package importing a private module should compile"
    );

    // The repository has gone, and neither module cache holds it yet.
    let ws = make_workspace_hermetic(app_importing("gone", &tidy_modcache)?)?;
    std::fs::remove_dir_all(repos.join("gone"))?;
    let err = ws.run("//:build_lib@v=host").await.err();
    let msg = format!("{:#?}", err.expect("an unreachable module must fail"));
    assert!(msg.contains("go list failed"), "{msg}");
    assert!(msg.contains("example.test/acme/gone.git"), "{msg}");
    assert!(
        !msg.contains("could not import"),
        "a failed fetch must not read as a missing package: {msg}"
    );

    // `go list` leaves its module cache read-only, and TempDir cannot delete that.
    Command::new("chmod")
        .args(["-R", "u+w"])
        .arg(scratch.path())
        .status()?;
    Ok(())
}
