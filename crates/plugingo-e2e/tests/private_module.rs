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
//! `GOPRIVATE` comes from a `go env -w` file, which is how many people set it. The
//! same file sets a `GOFLAGS` that must not reach a listing.
//!
//! A file of its own because it sets process-wide variables: this test binary holds
//! this one test, so nothing races those writes.

mod common;

use common::{HERMETIC_GO, artifact_paths, make_workspace_hermetic, require_go};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// A repository holding one module tagged `v1.0.0`, with one file per
/// `(path, content)`.
fn tagged_repo(dir: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
    }
    git(dir, &["init", "-q", "-b", "main", "."])?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-qm", "init"])?;
    git(dir, &["tag", "v1.0.0"])
}

/// `go list` leaves its module cache read-only, which `TempDir` cannot delete, and
/// this has to run however the test ends.
struct MakeWritable(PathBuf);

impl Drop for MakeWritable {
    fn drop(&mut self) {
        if let Err(e) = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.0)
            .status()
        {
            eprintln!("chmod {}: {e}", self.0.display());
        }
    }
}

const GONE_FILES: &[(&str, &str)] = &[
    ("go.mod", "module example.test/acme/gone.git\n\ngo 1.21\n"),
    ("gone.go", "package gone\n\nconst V = \"gone\"\n"),
];

#[tokio::test]
async fn private_modules_fetched_with_git_under_the_hermetic_sdk() -> anyhow::Result<()> {
    require_go!();
    if which::which("git").is_err() {
        eprintln!("skipping private module e2e: no git on PATH");
        return Ok(());
    }

    let scratch = tempfile::tempdir()?;
    let _writable = MakeWritable(scratch.path().to_path_buf());
    let (repos, home) = (scratch.path().join("repos"), scratch.path().join("home"));
    std::fs::create_dir_all(&home)?;

    let go_env_file = home.join("goenv");
    std::fs::write(
        &go_env_file,
        "GOPRIVATE=example.test\nGOFLAGS=-tags=enterprise\n",
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
    unsafe { std::env::set_var("HOME", &home) };
    // SAFETY: as above.
    unsafe { std::env::set_var("GOENV", &go_env_file) };
    for inherited in [
        "GOFLAGS",
        "GOMODCACHE",
        "GOPATH",
        "GOPRIVATE",
        "GONOPROXY",
        "GONOSUMDB",
        "GOPROXY",
        "GOSUMDB",
        "GOVCS",
        "GIT_CONFIG_GLOBAL",
        "XDG_CONFIG_HOME",
    ] {
        // SAFETY: as above.
        unsafe { std::env::remove_var(inherited) };
    }

    // `a` imports `b`, so listing and compiling `a` has to find `b` too. `b` has a
    // package no build selects, and the `empty` directory of `a` holds no package.
    tagged_repo(
        &repos.join("b"),
        &[
            ("go.mod", "module example.test/acme/b.git\n\ngo 1.21\n"),
            ("b.go", "package b\n\nconst V = \"b\"\n"),
            ("excluded/x.go", "//go:build ignore\n\npackage excluded\n"),
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
            ("empty/notes.txt", "no Go here\n"),
        ],
    )?;
    tagged_repo(&repos.join("gone"), GONE_FILES)?;

    let app = tempfile::tempdir()?;
    std::fs::write(
        app.path().join("go.mod"),
        "module example.test/app\n\ngo 1.21\n\n\
         require (\n\texample.test/acme/a.git v1.0.0\n\texample.test/acme/gone.git v1.0.0\n)\n",
    )?;
    std::fs::write(
        app.path().join("app.go"),
        "package app\n\nimport \"example.test/acme/a.git\"\n\nconst V = a.V\n",
    )?;
    // Redeclares `V`: this package only compiles if the file's `GOFLAGS` stayed out.
    std::fs::write(
        app.path().join("enterprise.go"),
        "//go:build enterprise\n\npackage app\n\nconst V = \"enterprise\"\n",
    )?;
    std::fs::create_dir_all(app.path().join("usesgone"))?;
    std::fs::write(
        app.path().join("usesgone/usesgone.go"),
        "package usesgone\n\nimport \"example.test/acme/gone.git\"\n\nconst V = gone.V\n",
    )?;
    // The go.mod and go.sum a real project has checked in. This fills its own
    // module cache, so `gone` is in neither cache heph uses.
    let tidy = Command::new("go")
        .args(["mod", "tidy"])
        .env("GOFLAGS", "-modcacherw")
        .env("GOMODCACHE", scratch.path().join("tidy-modcache"))
        .env("GOTOOLCHAIN", "local")
        .env("GOWORK", "off")
        .current_dir(app.path())
        .output()?;
    anyhow::ensure!(
        tidy.status.success(),
        "go mod tidy: {}",
        String::from_utf8_lossy(&tidy.stderr)
    );

    let ws = make_workspace_hermetic(app)?;
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

    // The real `go`'s wording for a module's contents must still read as "no such
    // package", not as a failed fetch: these are the phrases the check allow-lists.
    for no_package in [
        "//@heph/go/thirdparty/example.test/acme/b.git@v1.0.0/excluded:_golist@v=host",
        "//@heph/go/thirdparty/example.test/acme/a.git@v1.0.0/empty:_golist@v=host",
    ] {
        ws.run(no_package)
            .await
            .map_err(|e| anyhow::anyhow!("{no_package} should list as no package: {e:?}"))?;
    }

    // The repository is out of reach, and no module cache heph uses holds it.
    let gone_repo = repos.join("gone");
    let parked = repos.join("parked");
    std::fs::rename(&gone_repo, &parked)?;
    let err = ws.run("//usesgone:build_lib@v=host").await.err();
    let msg = format!("{:#?}", err.expect("an unreachable module must fail"));
    assert!(msg.contains("go list failed"), "{msg}");
    assert!(msg.contains("example.test/acme/gone.git"), "{msg}");
    assert!(
        !msg.contains("could not import"),
        "a failed fetch must not read as a missing package: {msg}"
    );

    // Access comes back and the user runs heph again: a new engine over the same
    // on-disk cache, which must not have recorded the failure.
    std::fs::rename(&parked, &gone_repo)?;
    let dir = ws.dir;
    drop(ws.engine);
    let ws = make_workspace_hermetic(dir)?;
    let result = ws
        .run("//usesgone:build_lib@v=host")
        .await
        .map_err(|e| anyhow::anyhow!("retry after access came back: {e:#?}"))?;
    assert!(
        !artifact_paths(&result).is_empty(),
        "a failed fetch must not be cached"
    );
    Ok(())
}
